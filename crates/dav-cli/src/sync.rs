//! `dav sync`: the account's whole calendar pass through the engine's own facade, into a
//! throwaway in-memory store, twice.
//!
//! The first pass starts from nothing; the second runs with nothing changed, which is the pass
//! a host repeats all day. What it prints per calendar is what a host's refresh does with it:
//! fetched, skipped because its tag had not moved, or failed and why.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engine_api::{
    AccountId, CalendarId, CalendarSyncReport, Engine, EventScopePass, Horizon, TimeZoneId,
    UtcDateTime,
};
use provider_caldav::CalDavProvider;

/// Syncs every calendar twice and prints what each pass did.
pub(crate) async fn sync(provider: &CalDavProvider) -> Result<(), String> {
    let engine = Engine::open_in_memory().map_err(|err| format!("store: {err}"))?;
    let account = AccountId::try_from("dav-cli").map_err(|err| err.to_string())?;
    let zone = TimeZoneId::iana("Europe/Amsterdam").map_err(|err| err.to_string())?;
    let horizon = this_year_and_next()?;
    for pass in ["first sync", "again, with nothing changed"] {
        let started = Instant::now();
        let report = engine
            .sync_calendar(provider, &account, horizon, &zone)
            .await
            .map_err(|err| format!("sync failed: {err}"))?;
        println!("{pass}: {} ms", started.elapsed().as_millis());
        print_report(&report);
    }
    Ok(())
}

fn print_report(report: &CalendarSyncReport) {
    for scope in &report.scopes {
        let calendars: Vec<&str> = scope.calendars.iter().map(CalendarId::as_str).collect();
        let outcome = match &scope.result {
            Ok(EventScopePass::Unchanged) => "unchanged, not asked".to_owned(),
            Ok(EventScopePass::Synced(events)) => format!(
                "synced: {} stored, {} removed",
                events.applied.upserted, events.applied.tombstoned
            ),
            Err(err) => format!("FAILED: {err}"),
        };
        println!("  {}  {outcome}", calendars.join(", "));
    }
    println!();
}

/// From the first of January this year to the first of January the year after next.
fn this_year_and_next() -> Result<Horizon, String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| err.to_string())?
        .as_secs();
    // Close enough for a window that only has to cover the events worth looking at.
    let year = i32::try_from(1970 + seconds / 31_556_952).map_err(|err| err.to_string())?;
    let start = UtcDateTime::new(year, 1, 1, 0, 0, 0).map_err(|err| err.to_string())?;
    let end = UtcDateTime::new(year + 2, 1, 1, 0, 0, 0).map_err(|err| err.to_string())?;
    Horizon::new(start, end).map_err(|err| err.to_string())
}
