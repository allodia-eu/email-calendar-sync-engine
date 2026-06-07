//! Argument parsing and command dispatch.
//!
//! Kept in the library (not `main.rs`) so the whole CLI surface — flag parsing,
//! command dispatch, and rendered output — is testable; the binary is a thin shim
//! that prints [`run`]'s output. Commands return their output as a string rather
//! than printing, so tests can assert it.

use std::collections::HashMap;

use engine_core::ids::AccountId;
use engine_core::time::{TimeZoneId, UtcDateTime};

use crate::{
    CliError, Fixture, Horizon, ingest, open, reexpand_calendar, search_calendar, search_mail,
};

/// The usage banner, shown for `--help`-less misuse.
pub const USAGE: &str = "\
usage:
  engine-cli ingest   --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>] <fixture.json>
  engine-cli reexpand --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>]
  engine-cli search   --db <path> --account <id> --kind <mail|calendar> [--limit <n>] <query...>";

/// Parses `args` (the arguments after the program name) and runs the command,
/// returning the output to print.
///
/// # Errors
///
/// Returns [`CliError::Usage`] for an unknown/missing command or bad flags, or a
/// pipeline error otherwise.
pub async fn run(args: &[String]) -> Result<String, CliError> {
    let Some((command, rest)) = args.split_first() else {
        return Err(CliError::Usage(USAGE.to_owned()));
    };
    let flags = Flags::parse(rest)?;
    match command.as_str() {
        "ingest" => cmd_ingest(&flags).await,
        "reexpand" => cmd_reexpand(&flags).await,
        "search" => cmd_search(&flags).await,
        other => Err(CliError::Usage(format!(
            "unknown command {other:?}\n{USAGE}"
        ))),
    }
}

async fn cmd_ingest(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let horizon = flags.horizon()?;
    let zone = flags.zone()?;
    let path = flags
        .positionals
        .first()
        .ok_or_else(|| CliError::Usage("ingest needs a fixture path".to_owned()))?;
    let json = std::fs::read_to_string(path).map_err(|e| CliError::Fixture(e.to_string()))?;
    let fixture = Fixture::from_json(&json)?;
    let store = open(flags.require("db")?)?;
    let report = ingest(&store, account, &fixture, &horizon, &zone).await?;
    Ok(format!(
        "ingested {} messages, {} events, {} occurrences",
        report.messages, report.events, report.occurrences
    ))
}

async fn cmd_reexpand(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let horizon = flags.horizon()?;
    let zone = flags.zone()?;
    let store = open(flags.require("db")?)?;
    let occurrences = reexpand_calendar(&store, account, &horizon, &zone).await?;
    Ok(format!("re-expanded {occurrences} occurrences"))
}

async fn cmd_search(flags: &Flags) -> Result<String, CliError> {
    let account = flags.account()?;
    let limit = flags.limit();
    let query = flags.positionals.join(" ");
    let store = open(flags.require("db")?)?;
    let results = match flags.require("kind")? {
        "mail" => search_mail(&store, account, &query, limit).await?,
        "calendar" => search_calendar(&store, account, &query, limit).await?,
        other => {
            return Err(CliError::Usage(format!(
                "--kind must be mail|calendar, got {other:?}"
            )));
        }
    };
    let mut lines: Vec<String> = results
        .hits
        .iter()
        .map(|hit| format!("{}\t{:.4}", hit.key.as_str(), hit.score))
        .collect();
    lines.push(format!(
        "coverage: complete={}",
        results.coverage.is_complete()
    ));
    Ok(lines.join("\n"))
}

/// A minimal `--flag value` plus positionals parser.
struct Flags {
    map: HashMap<String, String>,
    positionals: Vec<String>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut map = HashMap::new();
        let mut positionals = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if let Some(flag) = arg.strip_prefix("--") {
                let value = iter
                    .next()
                    .ok_or_else(|| CliError::Usage(format!("--{flag} needs a value")))?;
                map.insert(flag.to_owned(), value.clone());
            } else {
                positionals.push(arg.clone());
            }
        }
        Ok(Self { map, positionals })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    fn require(&self, key: &str) -> Result<&str, CliError> {
        self.get(key)
            .ok_or_else(|| CliError::Usage(format!("--{key} is required")))
    }

    fn account(&self) -> Result<AccountId, CliError> {
        AccountId::try_from(self.require("account")?)
            .map_err(|_| CliError::Usage("--account is not a valid account id".to_owned()))
    }

    fn zone(&self) -> Result<TimeZoneId, CliError> {
        let name = self.get("zone").unwrap_or("Etc/UTC");
        TimeZoneId::iana(name).map_err(|_| CliError::Usage("--zone must not be empty".to_owned()))
    }

    fn limit(&self) -> usize {
        self.get("limit").and_then(|s| s.parse().ok()).unwrap_or(20)
    }

    /// The expansion horizon, defaulting to a wide window when unspecified.
    fn horizon(&self) -> Result<Horizon, CliError> {
        let start = self.day_instant("horizon-start", "2020-01-01")?;
        let end = self.day_instant("horizon-end", "2030-01-01")?;
        Ok(Horizon::new(start, end)?)
    }

    /// Parses a `YYYY-MM-DD` flag into the UTC midnight instant, or `default`.
    fn day_instant(&self, key: &str, default: &str) -> Result<UtcDateTime, CliError> {
        let date = self.get(key).unwrap_or(default);
        format!("{date}T00:00:00Z")
            .parse()
            .map_err(|_| CliError::Usage(format!("--{key} must be YYYY-MM-DD")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::calendar::{Event, Frequency, Recurrence, RecurrenceRule};
    use engine_core::ids::{CalendarId, EventId, Uid};
    use engine_core::membership::Memberships;
    use engine_core::time::{CalendarDateTime, LocalDateTime};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn write_fixture(dir: &std::path::Path) -> String {
        let mut event = Event::new(
            EventId::try_from("daily").unwrap(),
            Uid::new("u-daily").unwrap(),
            Memberships::of_one(CalendarId::try_from("cal").unwrap()),
            CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
        );
        event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
        let json = serde_json::to_string(&serde_json::json!({ "events": [event] })).unwrap();
        let path = dir.join("fixture.json");
        std::fs::write(&path, json).unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn ingest_then_search_via_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let db = db.to_str().unwrap();
        let fixture = write_fixture(dir.path());

        let out = run(&args(&[
            "ingest",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-01",
            "--horizon-end",
            "2026-06-04",
            &fixture,
        ]))
        .await
        .unwrap();
        assert!(out.contains("3 occurrences"), "{out}");

        let found = run(&args(&[
            "search",
            "--db",
            db,
            "--account",
            "acct-1",
            "--kind",
            "calendar",
            "after:2026-06-01 before:2026-06-04",
        ]))
        .await
        .unwrap();
        assert!(found.contains("daily"));
        assert!(found.contains("complete=true"));

        let occ = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-01",
            "--horizon-end",
            "2026-06-10",
        ]))
        .await
        .unwrap();
        assert!(occ.contains("re-expanded 9 occurrences"), "{occ}");
    }

    #[tokio::test]
    async fn usage_and_flag_errors() {
        assert!(matches!(run(&[]).await, Err(CliError::Usage(_))));
        assert!(matches!(
            run(&args(&["frobnicate"])).await,
            Err(CliError::Usage(_))
        ));
        // A flag missing its value.
        assert!(matches!(
            run(&args(&["ingest", "--db"])).await,
            Err(CliError::Usage(_))
        ));
        // Missing required --account.
        assert!(matches!(
            run(&args(&["ingest", "--db", "x.sqlite"])).await,
            Err(CliError::Usage(_))
        ));
        // Bad search kind.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let bad_kind = run(&args(&[
            "search",
            "--db",
            db.to_str().unwrap(),
            "--account",
            "acct-1",
            "--kind",
            "contacts",
            "q",
        ]))
        .await;
        assert!(matches!(bad_kind, Err(CliError::Usage(_))));
    }

    #[tokio::test]
    async fn bad_horizon_and_missing_fixture_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.sqlite");
        let db = db.to_str().unwrap();
        // horizon-start after horizon-end is an empty window.
        let empty = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "2026-06-10",
            "--horizon-end",
            "2026-06-01",
        ]))
        .await;
        assert!(empty.is_err());
        // A malformed horizon date.
        let bad_date = run(&args(&[
            "reexpand",
            "--db",
            db,
            "--account",
            "acct-1",
            "--horizon-start",
            "nonsense",
        ]))
        .await;
        assert!(matches!(bad_date, Err(CliError::Usage(_))));
        // A fixture path that does not exist.
        let missing = run(&args(&[
            "ingest",
            "--db",
            db,
            "--account",
            "acct-1",
            "/no/such/fixture.json",
        ]))
        .await;
        assert!(matches!(missing, Err(CliError::Fixture(_))));
    }
}
