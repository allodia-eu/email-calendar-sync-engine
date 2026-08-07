//! Named server profiles: "which server, as whom" resolved once, by name.
//!
//! Every live investigation used to start the same way — grep for a port, grep for a
//! password, grep for the DAV path, get one of them wrong. A profile is the answer to
//! "*which server, as whom*" written down once, so a command line says `--profile soverin`
//! and nothing else.
//!
//! # Where they live, and why it is outside every checkout
//!
//! ```text
//! ~/.config/allodia/servers/<name>.env
//! ```
//!
//! Outside the repos on purpose. **They hold passwords**, so they may never sit where a
//! `git add -A` can reach them; and the engine and the product core are worked on in
//! parallel, so a profile written while debugging one must be usable from the other without
//! being copied. One directory, read by both.
//!
//! # The schema
//!
//! ```text
//! URL=https://caldav.example.net     # required — the DAV base, or a `.well-known` host
//! USER=someone@example.net           # required
//! PASS=…                             # required
//! CALENDAR=MyCalendar-9b361b6b-…     # optional — bind one collection instead of `default`
//! ```
//!
//! `CALENDAR` is the one that is easy to skip and painful to omit: a real account's
//! collection is rarely called `default`, and the failure is a `404` from deep inside a sync
//! that reads like a broken adapter.
//!
//! # Built-ins, and the line they do not cross
//!
//! Two profiles need no file because they are **this repository's own fixtures**:
//! `stalwart` (the auto-scheduling harness) and `sabredav` (the no-scheduling fixture). Their
//! addresses come from `STALWART_HTTP_ADDR` / `SABREDAV_HTTP_ADDR` when set — the same
//! variables the live tests read — and otherwise from the ports `docker/*/docker-compose.yml`
//! publishes.
//!
//! **Nothing else is built in.** The product core runs *its own* Stalwart harness, on its own
//! ports, as a deliberately separate compose project — but that is the core's fixture, not
//! this repo's, and hard-coding its ports here would put product knowledge into a
//! product-neutral engine. It gets a profile file like any other server, which is exactly
//! what the profile mechanism is for: both harnesses reachable, neither one assumed.

use std::{collections::BTreeMap, fmt, fs, path::PathBuf};

/// A resolved server: where it is, who we are, and which collection to bind.
#[derive(Debug, Clone)]
pub(crate) struct Profile {
    /// Where the profile came from, for the banner — a file path, a built-in name, or flags.
    pub(crate) origin: String,
    /// The DAV base URL.
    pub(crate) url: String,
    /// The username.
    pub(crate) user: String,
    /// The password.
    pub(crate) pass: String,
    /// The collection to bind, when the account's is not called `default`.
    pub(crate) calendar: Option<String>,
}

/// A profile could not be resolved, with a message naming what to fix.
#[derive(Debug)]
pub(crate) struct ProfileError(String);

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The directory profiles live in.
pub(crate) fn profile_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".config/allodia/servers")
}

/// The address a built-in fixture is reachable at, given whatever its env var said.
///
/// Split from [`fixture`] so the rule is testable: reading the environment inside it would
/// make every assertion depend on the shell that launched the test.
fn fixture_url(configured: Option<&str>, fallback: &str) -> String {
    format!("http://{}", configured.unwrap_or(fallback))
}

/// The address a built-in fixture is reachable at: its env var, else the published port.
///
/// It follows the same variable the live suite reads, so the tool and a failing test always
/// point at the same server.
fn fixture(var: &str, fallback: &str) -> String {
    fixture_url(std::env::var(var).ok().as_deref(), fallback)
}

/// The profiles this repository ships, because they are its own docker fixtures.
fn built_in(name: &str) -> Option<Profile> {
    let (url, user, pass) = match name {
        "stalwart" => (
            fixture("STALWART_HTTP_ADDR", "127.0.0.1:18080"),
            "carol@test.local",
            "harness-carol-pw",
        ),
        "stalwart-organizer" => (
            fixture("STALWART_HTTP_ADDR", "127.0.0.1:18080"),
            "bob@test.local",
            "harness-bob-pw",
        ),
        "sabredav" => (
            fixture("SABREDAV_HTTP_ADDR", "127.0.0.1:18081"),
            "alice@test.local",
            "sabredav-alice-pw",
        ),
        _ => return None,
    };
    Some(Profile {
        origin: format!("built-in `{name}`"),
        url,
        user: user.to_owned(),
        pass: pass.to_owned(),
        calendar: None,
    })
}

/// The built-in profile names, for `profiles` and for an error message that can suggest one.
pub(crate) const BUILT_INS: [&str; 3] = ["stalwart", "stalwart-organizer", "sabredav"];

/// `KEY=value` lines. Comments, blanks, `export ` and surrounding quotes are tolerated;
/// nothing is interpolated and nothing is executed — a credentials file is a few lines, and a
/// parser that can run something is a parser that can be made to run something.
fn parse(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        values.insert(key.trim().to_owned(), value.to_owned());
    }
    values
}

/// Every profile that could be used right now: the built-in fixtures plus every file.
pub(crate) fn available() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = BUILT_INS
        .iter()
        .map(|name| ((*name).to_owned(), "built-in fixture".to_owned()))
        .collect();
    if let Ok(entries) = fs::read_dir(profile_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "env")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                found.push((stem.to_owned(), path.display().to_string()));
            }
        }
    }
    found.sort();
    found
}

/// Resolves `name` to a profile: a file in the profile directory, else a built-in fixture.
///
/// A file **wins** over a built-in of the same name, so a real deployment can be given a
/// familiar name without this tool arguing about it.
pub(crate) fn load(name: &str) -> Result<Profile, ProfileError> {
    let path = profile_dir().join(format!("{name}.env"));
    if !path.is_file() {
        return built_in(name).ok_or_else(|| {
            ProfileError(format!(
                "no profile `{name}`: no file at {}, and it is not one of the built-in \
                 fixtures ({}).\nWrite the file with URL=, USER=, PASS= and optionally \
                 CALENDAR=, then `chmod 600` it — see `dav profiles`.",
                path.display(),
                BUILT_INS.join(", "),
            ))
        });
    }
    warn_if_readable_by_others(&path);
    let text = fs::read_to_string(&path)
        .map_err(|err| ProfileError(format!("cannot read {}: {err}", path.display())))?;
    let values = parse(&text);
    let require = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| ProfileError(format!("{} does not set {key}", path.display())))
    };
    Ok(Profile {
        origin: path.display().to_string(),
        url: require("URL")?,
        user: require("USER")?,
        pass: require("PASS")?,
        calendar: values.get("CALENDAR").cloned(),
    })
}

/// A one-line nudge if a file holding a password is group- or world-readable.
fn warn_if_readable_by_others(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            eprintln!(
                "warning: {} is readable by other users and holds a password. \
                 chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
