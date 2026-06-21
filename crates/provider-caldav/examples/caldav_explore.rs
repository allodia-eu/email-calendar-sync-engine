//! Explore a live CalDAV account: list calendars and the bound calendar's events.
//!
//! **Read-only** — it discovers the calendar home, lists the account's calendars,
//! then syncs the bound collection's events and prints each one's start, kind, and
//! title. It validates the `provider-caldav` client against a *real* CalDAV server
//! (Fastmail/iCloud/Google over HTTPS, or a local server such as the Stalwart
//! harness over plain HTTP). This is the calendar parallel to `provider-imap`'s
//! `imap_explore` — the external-provider smoke test `north-star.md` step 7
//! anticipates, ahead of schedule. CalDAV writes are not implemented yet, so this
//! example does not mutate anything.
//!
//! Credentials come from the environment — never hard-code or paste a password:
//!
//! ```sh
//! export CALDAV_URL=https://caldav.example.com CALDAV_USER=you@example.com
//! read -rs CALDAV_PASS; export CALDAV_PASS   # type the password (no echo)
//! cargo run -p provider-caldav --example caldav_explore
//! # optional: CALDAV_CALENDAR=default            (the collection to bind events to)
//! # optional: CALDAV_DISCOVERY=/.well-known/caldav (discovery start path)
//! #
//! # Against the local Stalwart harness (docker/stalwart up):
//! #   export CALDAV_URL=http://127.0.0.1:18080 \
//! #          CALDAV_USER=alice@test.local CALDAV_PASS=harness-alice-pw
//! #   cargo run -p provider-caldav --example caldav_explore
//! ```

use std::env;

use engine_core::ids::AccountId;
use engine_core::sync::SyncUpdate;
use engine_core::time::CalendarDateTime;
use engine_provider::Provider;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(url), Ok(user), Ok(pass)) = (
        env::var("CALDAV_URL"),
        env::var("CALDAV_USER"),
        env::var("CALDAV_PASS"),
    ) else {
        eprintln!("Set CALDAV_URL, CALDAV_USER, CALDAV_PASS to run. For example:");
        eprintln!("  export CALDAV_URL=https://caldav.example.com CALDAV_USER=you@example.com");
        eprintln!("  read -rs CALDAV_PASS; export CALDAV_PASS   # type the password, no echo");
        eprintln!("  cargo run -p provider-caldav --example caldav_explore");
        return Ok(());
    };
    // A pinned collection (CALDAV_CALENDAR) is respected as-is; otherwise the
    // library default is "default", and we auto-select a real calendar below.
    let pinned = env::var("CALDAV_CALENDAR").ok();
    let mut config = CalDavConfig::new(
        url.clone(),
        Credentials::Basic {
            username: user.clone(),
            password: pass,
        },
    );
    if let Some(calendar) = pinned.clone() {
        config = config.with_calendar(calendar);
    }
    if let Ok(path) = env::var("CALDAV_DISCOVERY") {
        config = config.with_discovery_path(path);
    }

    println!("Connecting to {url} as {user}…");
    let provider = CalDavProvider::connect(config).await?;

    let account = AccountId::try_from("explore").expect("account id");

    // List the account's calendars (re-discovered as a snapshot each pass).
    let calendars = provider.sync_calendars(&account, None).await?;
    let cals = objects(&calendars.update);
    println!("\n{} calendar(s) in the home:", cals.len());
    for cal in cals {
        println!("  • {}  [{}]", cal.name, cal.id.as_str());
    }

    // Read events from the bound collection if it exists (or the user pinned one);
    // otherwise fall back to the first discovered calendar, so the example works
    // against a server whose collections are not named "default". The fallback
    // `rebind`s without re-running discovery (the home is already known).
    let bound_exists = cals
        .iter()
        .any(|c| c.id.as_str() == provider.collection_href());
    let provider = if pinned.is_some() || bound_exists {
        provider
    } else if let Some(first) = cals.first() {
        let (target, name) = (first.id.as_str().to_owned(), first.name.clone());
        println!(
            "\n(The bound '{}' was not found; reading events from '{name}'.)",
            provider.collection_href(),
        );
        provider.rebind(&target)?
    } else {
        println!("\nNo calendars in the home to read events from.");
        return Ok(());
    };
    println!("\nReading events from: {}", provider.collection_href());

    // Sync the bound calendar's events (a full snapshot — no prior cursor).
    let events = provider.sync_events(&account, None).await?;
    let evs = objects(&events.update);
    println!("\n{} event(s):", evs.len());
    for ev in evs.iter().take(25) {
        let kind = if ev.is_recurring() {
            "recurring"
        } else if ev.is_override_instance() {
            "override"
        } else {
            "single"
        };
        let title = if ev.title.is_empty() {
            "(no title)"
        } else {
            ev.title.as_str()
        };
        println!("  • {:<24}  {kind:<9}  {title}", describe_start(&ev.start));
    }
    println!("\nNext sync-token cursor: {}", events.next_cursor.as_str());
    Ok(())
}

/// The created-or-updated objects an update carries.
fn objects<T>(update: &SyncUpdate<T>) -> &[T] {
    match update {
        SyncUpdate::Delta { changed, .. } => changed,
        SyncUpdate::Snapshot { objects, .. } => objects,
    }
}

/// A human-readable one-line rendering of an event start.
fn describe_start(start: &CalendarDateTime) -> String {
    match start {
        CalendarDateTime::Date(date) => format!("{date} (all-day)"),
        CalendarDateTime::Floating(local) => format!("{local} (floating)"),
        CalendarDateTime::Zoned { local, zone } => format!("{local} {zone}"),
    }
}
