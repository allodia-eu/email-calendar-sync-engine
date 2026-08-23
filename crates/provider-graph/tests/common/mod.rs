//! Shared setup for the gated `provider-graph` live calendar suites.
//!
//! Its own module rather than a copy per suite: each needs the same token gate, the same
//! bound provider and the same hand-assembled base event, and a second copy is a second
//! thing to keep in step with the adapter.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
};
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient};

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
pub(crate) fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Calendar (gated live)
// ---------------------------------------------------------------------------

pub(crate) fn calendar_window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

pub(crate) fn amsterdam() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// A calendar provider bound to `calendar`, reading times in Europe/Amsterdam.
pub(crate) fn calendar_provider(token: &str, calendar: CalendarId) -> GraphCalendarProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphCalendarProvider::new(client, calendar, calendar_window(), amsterdam())
}

pub(crate) fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: amsterdam(),
    }
}

/// A minimal event carrying the identity + revision a write receipt reports, so a
/// follow-up patch/delete can guard on the ETag the create/patch returned.
pub(crate) fn base_from(
    receipt_event: &CalendarId,
    id: &str,
    uid: &Uid,
    revisions: engine_core::version::RevisionTokens,
) -> Event {
    let mut event = Event::new(
        engine_core::ids::EventId::try_from(id).unwrap(),
        uid.clone(),
        Memberships::of_one(receipt_event.clone()),
        zoned("2026-09-01T10:00:00"),
    );
    event.revisions = revisions;
    event
}
