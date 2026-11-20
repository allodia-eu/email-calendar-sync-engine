//! Shared setup for the gated `provider-graph` live suites.
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
    sync::SyncState,
    time::{CalendarDateTime, Horizon, LocalDateTime, TimeZoneId},
};
use engine_provider::{CalendarSync, ProviderResult, ScopeSync};
use provider_graph::{GraphCalendarProvider, GraphClient};

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

/// `[from, to)` as a horizon of whole UTC days.
pub(crate) fn days(from: time::Date, to: time::Date) -> Horizon {
    Horizon::new(
        format!("{from}T00:00:00Z").parse().unwrap(),
        format!("{to}T00:00:00Z").parse().unwrap(),
    )
    .unwrap()
}

pub(crate) fn calendar_window() -> Horizon {
    days(
        time::Date::from_calendar_date(2026, time::Month::August, 1).unwrap(),
        time::Date::from_calendar_date(2026, time::Month::November, 1).unwrap(),
    )
}

pub(crate) fn amsterdam() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// A provider bound to the window a live test reads its calendar over, which the engine names
/// per fetch ([`CalendarSync::sync_events`]). Everything but the fetch derefs to the provider.
pub(crate) struct Windowed {
    pub(crate) provider: GraphCalendarProvider,
    pub(crate) window: Horizon,
}

impl Windowed {
    /// The calendar's events since `cursor`, over this provider's window.
    pub(crate) async fn sync_events(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        CalendarSync::sync_events(&self.provider, account, calendar, cursor, self.window).await
    }
}

impl core::ops::Deref for Windowed {
    type Target = GraphCalendarProvider;

    fn deref(&self) -> &GraphCalendarProvider {
        &self.provider
    }
}

/// A calendar provider for the account's calendars, reading times in Europe/Amsterdam.
pub(crate) fn calendar_provider(token: &str) -> Windowed {
    Windowed {
        provider: graph_calendars(token),
        window: calendar_window(),
    }
}

/// A calendar provider whose read window contains a generated live-test date.
pub(crate) fn calendar_provider_around(token: &str, date: time::Date) -> Windowed {
    Windowed {
        provider: graph_calendars(token),
        window: days(
            date - time::Duration::days(1),
            date + time::Duration::days(1),
        ),
    }
}

/// The adapter for `token`'s calendars, reading times in Europe/Amsterdam.
pub(crate) fn graph_calendars(token: &str) -> GraphCalendarProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GraphCalendarProvider::new(client, amsterdam())
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
