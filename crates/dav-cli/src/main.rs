//! `dav` — talk to a real CalDAV server through the engine's own adapter.
//!
//! # Why this exists
//!
//! Debugging a live server used to mean writing a throwaway: a script with its own HTTP
//! client and its own iCalendar parser, or a temporary `#[tokio::test]` deleted an hour later.
//! Both answer questions about *themselves*. A script's parser can read a property the adapter
//! misses, or miss one it reads, and you learn something true about the script.
//!
//! So every command here but [`raw`] drives `CalDavProvider` — the same discovery, the same
//! sync, the same RSVP path the product uses. When it prints a verdict, that is the verdict a
//! host would get.
//!
//! # Usage
//!
//! ```text
//! dav [--profile NAME] [--url U --user U --pass P] [--calendar C] <command>
//!
//!   profiles                      what is configured, and where it came from
//!   info                          connect, and print what discovery concluded
//!   list                          events, with each one's stored reply verdict
//!   get <uid>                     the stored iCalendar document, verbatim
//!   store <file.eml|.ics>         put an invitation on the calendar (guarded create)
//!   rsvp <uid> <accept|decline|tentative>    answer, and print the delivery verdict
//!   raw <METHOD> <href> [--depth N] [--body FILE|allprop]   outside the adapter
//! ```
//!
//! `--profile` names a file in `~/.config/allodia/servers/` or a built-in fixture; see
//! [`profile`]. Explicit `--url/--user/--pass` override it.
//!
//! # It writes to real servers
//!
//! `store` and `rsvp` change a real calendar, and on an auto-scheduling server `rsvp` **emails
//! the organizer**. Both say what they are about to do before they do it. Point them at a test
//! account.

mod commands;
mod profile;
mod raw;

use engine_provider::RsvpResponse;

use crate::profile::Profile;

/// The parsed command line: the connection, then what to do with it.
struct Invocation {
    profile: Profile,
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print_usage();
        return std::process::ExitCode::SUCCESS;
    }
    if args[0] == "profiles" {
        list_profiles();
        return std::process::ExitCode::SUCCESS;
    }
    match run(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!("{}", include_str!("usage.txt"));
}

/// Prints every profile that could be used right now, and where each came from.
fn list_profiles() {
    println!("profile directory: {}\n", profile::profile_dir().display());
    for (name, origin) in profile::available() {
        println!("  {name:<24} {origin}");
    }
    println!(
        "\nA profile file sets URL=, USER=, PASS= and optionally CALENDAR=.\n\
         Keep it mode 600 — it holds a password. A file wins over a built-in of the same name.\n\n\
         CALENDAR= is the one people skip: a real account's collection is rarely called\n\
         `default`, and omitting it fails as a 404 from inside a sync, which reads like a bug."
    );
}

/// Pulls the connection flags off the front of the argument list.
fn parse_invocation(args: Vec<String>) -> Result<Invocation, String> {
    let mut named: Option<String> = None;
    let (mut url, mut user, mut pass, mut calendar) = (None, None, None, None);
    let mut rest = Vec::new();

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let mut take = |what: &str| iter.next().ok_or_else(|| format!("--{what} needs a value"));
        match arg.as_str() {
            "--profile" | "-p" => named = Some(take("profile")?),
            "--url" => url = Some(take("url")?),
            "--user" => user = Some(take("user")?),
            "--pass" => pass = Some(take("pass")?),
            "--calendar" => calendar = Some(take("calendar")?),
            _ => rest.push(arg),
        }
    }

    let mut profile = match &named {
        Some(name) => profile::load(name).map_err(|err| err.to_string())?,
        None => Profile {
            origin: "flags".to_owned(),
            url: String::new(),
            user: String::new(),
            pass: String::new(),
            calendar: None,
        },
    };
    if let Some(value) = url {
        profile.url = value;
    }
    if let Some(value) = user {
        profile.user = value;
    }
    if let Some(value) = pass {
        profile.pass = value;
    }
    if calendar.is_some() {
        profile.calendar = calendar;
    }
    if profile.url.is_empty() || profile.user.is_empty() {
        return Err(
            "no server: pass --profile NAME, or --url/--user/--pass. `dav profiles` lists what \
             is configured."
                .to_owned(),
        );
    }
    if rest.is_empty() {
        return Err("no command — try `dav --help`".to_owned());
    }
    Ok(Invocation {
        profile,
        command: rest,
    })
}

async fn run(args: Vec<String>) -> Result<(), String> {
    let Invocation { profile, command } = parse_invocation(args)?;
    let argument = |index: usize| command.get(index).map(String::as_str);

    println!("server    {} ({})", profile.url, profile.origin);
    println!("as        {}", profile.user);
    if let Some(calendar) = &profile.calendar {
        println!("calendar  {calendar}");
    }
    println!();

    // `raw` deliberately skips discovery: it exists for servers where discovery is the thing
    // under suspicion, and a tool that cannot run until the adapter connects is no use there.
    if argument(0) == Some("raw") {
        let method = argument(1).ok_or("raw needs a METHOD")?;
        let href = argument(2).ok_or("raw needs an href")?;
        let depth = flag(&command, "--depth").unwrap_or_else(|| "0".to_owned());
        let body = match flag(&command, "--body") {
            Some(value) if value == "allprop" => raw::ALLPROP.to_owned(),
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|err| format!("cannot read {path}: {err}"))?,
            None => String::new(),
        };
        return raw::send(&profile, method, href, &depth, body).await;
    }

    let provider = commands::connect(&profile).await?;

    match argument(0) {
        Some("info") => {
            commands::describe(&provider);
            Ok(())
        }
        Some("list") => {
            let events = commands::events(&provider).await?;
            commands::print_events(&events, &profile.user);
            Ok(())
        }
        Some("get") => {
            let uid = argument(1).ok_or("get needs a UID")?;
            let events = commands::events(&provider).await?;
            let event = events
                .iter()
                .find(|event| event.uid.as_str() == uid)
                .ok_or_else(|| format!("no event with UID {uid}"))?;
            match &event.raw_ical {
                Some(raw) => println!("{}", raw.as_str()),
                None => return Err("the event carries no stored iCalendar".to_owned()),
            }
            Ok(())
        }
        Some("store") => {
            let path = argument(1).ok_or("store needs a file")?;
            commands::store(&provider, path).await
        }
        Some("rsvp") => {
            let uid = argument(1).ok_or("rsvp needs a UID")?;
            let response = match argument(2) {
                Some("accept") => RsvpResponse::Accepted,
                Some("decline") => RsvpResponse::Declined,
                Some("tentative") => RsvpResponse::Tentative,
                other => {
                    return Err(format!(
                        "rsvp needs accept|decline|tentative, got {}",
                        other.unwrap_or("nothing")
                    ));
                }
            };
            let events = commands::events(&provider).await?;
            commands::rsvp(&provider, &events, uid, response, &profile.user).await
        }
        other => Err(format!(
            "unknown command {} — try `dav --help`",
            other.unwrap_or("(none)")
        )),
    }
}

/// The value following `name` in the command words, if it is there.
fn flag(command: &[String], name: &str) -> Option<String> {
    command
        .iter()
        .position(|word| word == name)
        .and_then(|index| command.get(index + 1))
        .cloned()
}
