//! `provider-caldav` — the CalDAV (calendar) read/sync provider adapter.
//!
//! This is the calendar half of build-order step 5 (`north-star.md`): a CalDAV
//! client that syncs an account's calendars and events into the engine's
//! normalized [`Calendar`](engine_core::calendar::Calendar) /
//! [`Event`](engine_core::calendar::Event) projections, against the same Stalwart
//! fixture the JMAP and IMAP adapters use. It implements the
//! [`Provider`](engine_provider::Provider) contract, so the generic
//! `engine_sync::sync_calendar` loop drives it unchanged — the only differences
//! from JMAP are the transport (WebDAV over HTTP) and that the calendar payload
//! arrives as **iCalendar** (RFC 5545) which this crate parses, where JMAP
//! supplied JSCalendar directly. `caldav.md` is authoritative for the design.
//!
//! Layers (mirroring `provider-jmap`'s `Executor` seam so the whole protocol is
//! offline-testable by replaying captured transcripts):
//! - [`ical`] — the iCalendar parser: text → normalized
//!   [`Event`](engine_core::calendar::Event)s, folding a resource's master +
//!   `RECURRENCE-ID` override `VEVENT`s into one event.
//! - `dav` — the WebDAV `multistatus` XML parser.
//! - `transport` — the HTTP `DavExecutor` seam + its `reqwest` implementation.
//! - `discovery`/`calendar` — principal → calendar-home → collection listing.
//! - `sync` — the `sync-collection` REPORT (RFC 6578) snapshot/delta logic.
//! - `provider` — the [`Provider`](engine_provider::Provider) implementation.

mod calendar;
mod dav;
mod discovery;
mod error;
pub mod ical;
mod provider;
mod request;
mod sync;
#[cfg(test)]
mod test_support;
mod transport;

pub use error::CalDavError;
pub use provider::{CalDavConfig, CalDavProvider};
pub use transport::Credentials;

/// Parses arbitrary bytes as an iCalendar object resource through the full
/// normalize pipeline, discarding the result — the entry point for the `fuzz/`
/// cargo-fuzz target (mirrors `provider-jmap`'s `fuzz_parse`). The invariant is
/// panic-freedom on hostile input (`north-star.md` security). Hidden behind the
/// `fuzzing` feature.
#[cfg(feature = "fuzzing")]
pub fn fuzz_parse(data: &[u8]) {
    use engine_core::ids::{CalendarId, EventId};
    let (Ok(id), Ok(calendar)) = (
        EventId::try_from("/fuzz/r.ics"),
        CalendarId::try_from("/fuzz/"),
    ) else {
        return;
    };
    let _ = ical::parse_calendar_object(&String::from_utf8_lossy(data), id, calendar);
}
