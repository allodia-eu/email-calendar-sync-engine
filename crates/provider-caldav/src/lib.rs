//! `provider-caldav` — the CalDAV (calendar) read/sync **and write** provider
//! adapter.
//!
//! This is the calendar half of build-order step 5 (`north-star.md`): a CalDAV
//! client that syncs an account's calendars and events into the engine's
//! normalized [`Calendar`](engine_core::calendar::Calendar) /
//! [`Event`](engine_core::calendar::Event) projections, and writes events back with
//! conditional `PUT`/`DELETE`, against the same Stalwart fixture the JMAP and IMAP
//! adapters use. It implements the [`Provider`](engine_provider::Provider) contract,
//! so the generic `engine_sync::sync_calendar` loop drives reads unchanged and the
//! `engine_sync::write_calendar_event` outbox driver drives writes — the only
//! differences from JMAP are the transport (WebDAV over HTTP) and that the calendar
//! payload is **iCalendar** (RFC 5545) which this crate parses (reads) and carries
//! verbatim (writes), where JMAP supplied JSCalendar directly. `caldav.md` is
//! authoritative for the design.
//!
//! Layers (mirroring `provider-jmap`'s `Executor` seam so the whole protocol is
//! offline-testable by replaying captured transcripts):
//! - The iCalendar layer is **not** in this crate — it is `engine_ical`, because iCalendar also
//!   arrives over *mail* (iMIP) on accounts with no CalDAV in the picture at all. It parses text →
//!   normalized [`Event`](engine_core::calendar::Event)s, folding a resource's master +
//!   `RECURRENCE-ID` override `VEVENT`s into one event, and carries the two writers —
//!   `build_event_ical` (how CalDAV serializes a neutral `EventDraft`) and `patch_event_ical`, the
//!   structural patcher that edits a stored resource *in place* so an edit cannot delete the
//!   properties the projection does not model. Both stay **internal to this crate's write path**: a
//!   host states intent through the neutral write verbs (`engine_provider::EventDraft`/`EventEdit`)
//!   and never assembles iCalendar itself.
//! - `dav` — the WebDAV `multistatus` XML parser.
//! - `transport` — the HTTP `DavExecutor` seam (read reports + writes) + its `reqwest`
//!   implementation.
//! - `discovery`/`calendar` — principal → calendar-home → collection listing.
//! - `sync` — the `sync-collection` REPORT (RFC 6578) snapshot/delta logic.
//! - `write` — conditional `PUT`/`DELETE` (`If-Match`/`If-None-Match`) of event resources.
//! - [`imip`] — iMIP (iTIP over email, RFC 6047): parsing an inbound `text/calendar` scheduling
//!   message into an [`engine_core::scheduling::SchedulingMessage`], and the RSVP write primitive
//!   that patches my `PARTSTAT` into a stored event's raw for a conditional `PUT` back
//!   (`calendar-semantics.md`). This is the one caller of the whole-document write verb
//!   ([`EventWrite`](engine_provider::EventWrite)), because an RSVP is naturally a finished
//!   document rather than a property patch.
//! - `provider` — the [`Provider`](engine_provider::Provider) implementation.

mod calendar;
mod carddav;
mod carddav_ops;
mod dav;
mod discovery;
mod error;
mod href;
pub mod imip;
mod provider;
mod request;
mod sync;
#[cfg(test)]
mod test_support;
mod transport;
mod vcard;
mod vcard_escape;
mod vcard_property;
mod vcard_write;
mod write;

#[cfg(test)]
mod carddav_tests;
#[cfg(test)]
mod carddav_unit_tests;
#[cfg(test)]
mod vcard_tests;

pub use carddav::{CardDavConfig, CardDavProvider};
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
    let _ = engine_ical::parse_calendar_object(&String::from_utf8_lossy(data), id, calendar);
}
