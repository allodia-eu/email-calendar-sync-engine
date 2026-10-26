//! Explore a live CalDAV account: list its calendars and one calendar's events.
//!
//! **Read-only by default** — it discovers the calendar home, lists the account's
//! calendars with their collection tags, then syncs one calendar's events and prints each
//! one's start,
//! kind, and title. It validates the `provider-caldav` client against a *real*
//! CalDAV server (Fastmail/iCloud/Google over HTTPS, or a local server such as the
//! Stalwart harness over plain HTTP). This is the calendar parallel to
//! `provider-imap`'s `imap_explore` — the external-provider smoke test
//! `north-star.md` step 7 anticipates, ahead of schedule.
//!
//! Set `CALDAV_WRITE=1` to additionally run a **write demo**: it creates a
//! throwaway event in that calendar and immediately deletes it again, so the
//! calendar is left exactly as it was (the opt-in parallel to `imap_explore`'s
//! `IMAP_DRAFT`/`IMAP_SEND`).
//!
//! Credentials come from the environment — never hard-code or paste a password:
//!
//! ```sh
//! export CALDAV_URL=https://caldav.example.com CALDAV_USER=you@example.com
//! read -rs CALDAV_PASS; export CALDAV_PASS   # type the password (no echo)
//! cargo run -p provider-caldav --example caldav_explore
//! # optional: CALDAV_CALENDAR=default            (the calendar to read: href or last segment)
//! # optional: CALDAV_DISCOVERY=/.well-known/caldav (discovery start path)
//! # optional: CALDAV_WRITE=1                      (create + delete a throwaway event)
//! #
//! # Against the local Stalwart harness (docker/stalwart up):
//! #   export CALDAV_URL=http://127.0.0.1:18080 \
//! #          CALDAV_USER=alice@test.local CALDAV_PASS=harness-alice-pw
//! #   cargo run -p provider-caldav --example caldav_explore
//! ```

use std::env;

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::{SyncObject, SyncUpdate},
    time::CalendarDateTime,
};
use engine_provider::{CalendarWrites, Provider};
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
    let pinned = env::var("CALDAV_CALENDAR").ok();
    let mut config = CalDavConfig::new(
        url.clone(),
        Credentials::Basic {
            username: user.clone(),
            password: pass,
        },
        provider_caldav::UserAgent::new("caldav-explore", env!("CARGO_PKG_VERSION"))?,
    );
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
        let tag = cal.collection_tag.as_ref().map_or("—", |tag| tag.as_str());
        println!("  • {}  [{}]  tag {tag}", cal.name, cal.id.as_str());
    }

    // The calendar CALDAV_CALENDAR names (its href or last segment), else one called
    // "default", else the first listed.
    let segment = |cal: &&engine_core::calendar::Calendar| {
        cal.id
            .as_str()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .map(str::to_owned)
    };
    let wanted = pinned.as_deref().unwrap_or("default");
    let Some(calendar) = cals
        .iter()
        .find(|cal| cal.id.as_str() == wanted || segment(cal).as_deref() == Some(wanted))
        .or_else(|| pinned.is_none().then(|| cals.first()).flatten())
    else {
        println!("\nNo calendar {wanted:?} in the home to read events from.");
        return Ok(());
    };
    let calendar: CalendarId = calendar.id.clone();
    println!("\nReading events from: {}", calendar.as_str());

    // Sync the calendar's events (a full snapshot — no prior cursor).
    let events = provider.sync_events(&account, &calendar, None).await?;
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

    // Opt-in write demo: create a throwaway event, then delete it, leaving the
    // calendar untouched. Off by default so the example never mutates by accident.
    if env::var("CALDAV_WRITE").is_ok() {
        write_demo(&provider, &account, &calendar).await?;
    }
    Ok(())
}

/// Creates a throwaway event in `calendar`, then deletes it — a round trip that
/// leaves the calendar as it was.
///
/// Written the way a host writes: state the event, let the adapter serialize it. No
/// iCalendar is assembled here, and nothing in this function is CalDAV-specific except the
/// provider it is handed.
async fn write_demo(
    provider: &CalDavProvider,
    account: &AccountId,
    calendar: &CalendarId,
) -> Result<(), Box<dyn std::error::Error>> {
    use engine_core::{
        ids::Uid,
        time::{CalendarDateTime, TimeZoneId},
    };
    use engine_provider::{EventDeletion, EventDraft};

    let amsterdam = |local: &str| -> Result<CalendarDateTime, Box<dyn std::error::Error>> {
        Ok(CalendarDateTime::Zoned {
            local: local.parse()?,
            zone: TimeZoneId::iana("Europe/Amsterdam")?,
        })
    };

    let uid = Uid::new("caldav-explore-demo@example.invalid")?;
    println!("\nCALDAV_WRITE set — write demo:");
    // Clean up any leftover from a prior interrupted run, so the create is a true
    // create (If-None-Match: *).
    let _ = provider
        .delete_event(
            account,
            None,
            &EventDeletion::unconditional(provider.event_href(calendar, &uid)?, uid.clone()),
        )
        .await;

    let receipt = provider
        .create_event(
            account,
            &EventDraft::new(
                calendar.clone(),
                uid.clone(),
                "caldav_explore demo (safe to delete)",
                amsterdam("2026-06-01T10:00:00")?,
                amsterdam("2026-06-01T10:30:00")?,
                "2026-01-01T00:00:00Z".parse()?,
            ),
        )
        .await?;
    println!(
        "  • created {}  (etag {})",
        receipt.event.as_str(),
        receipt.revisions.etag.as_ref().map_or("—", |e| e.as_str())
    );

    provider
        .delete_event(
            account,
            None,
            &EventDeletion::unconditional(receipt.event, receipt.uid),
        )
        .await?;
    println!("  • deleted — the calendar is left as it was.");
    Ok(())
}

/// The created-or-updated objects an update carries.
fn objects<T: SyncObject>(update: &SyncUpdate<T>) -> &[T] {
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
