//! Shared setup for the gated `provider-google` live calendar suites.
//!
//! Its own module rather than a copy per suite: each needs the same token gate and the same
//! bound provider, and a second copy is a second thing to keep in step with the adapter.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId},
    sync::SyncState,
    time::{CalendarDateTime, Horizon, LocalDateTime, TimeZoneId},
};
use engine_provider::{CalendarWrites, Provider, ProviderResult, ScopeSync};
use provider_google::{GoogleCalendarProvider, GoogleClient};

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
pub(crate) fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

// --- Google Calendar (Phase D) ---

/// The window the suites read a calendar over: wide enough to be the whole of it, which is what
/// these suites read before the engine named a window per fetch.
pub(crate) fn whole_calendar() -> Horizon {
    Horizon::new(
        "2000-01-01T00:00:00Z".parse().unwrap(),
        "2100-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

/// A provider bound to the window a live test reads its calendar over, which the engine names
/// per fetch ([`Provider::sync_events`]). Everything but the fetch derefs to the provider.
pub(crate) struct Windowed {
    pub(crate) provider: GoogleCalendarProvider,
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
        Provider::sync_events(&self.provider, account, calendar, cursor, self.window).await
    }
}

impl core::ops::Deref for Windowed {
    type Target = GoogleCalendarProvider;

    fn deref(&self) -> &GoogleCalendarProvider {
        &self.provider
    }
}

/// A provider for `token`'s calendars, reading each calendar whole.
pub(crate) fn calendar_provider(token: String) -> Windowed {
    Windowed {
        provider: google_calendars(token),
        window: whole_calendar(),
    }
}

/// The adapter for `token`'s calendars.
pub(crate) fn google_calendars(token: String) -> GoogleCalendarProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    GoogleCalendarProvider::new(client)
}

/// The account's default calendar, by the id its listing gives it: the account's address.
/// Google also answers to `primary`, which the adapter refuses as a second id for it.
pub(crate) async fn primary(provider: &GoogleCalendarProvider) -> CalendarId {
    let addresses = provider
        .calendar_user_addresses(&account())
        .await
        .expect("the default calendar's address");
    CalendarId::try_from(addresses.addresses()[0].as_str()).unwrap()
}

/// A zoned wall clock in the zone the probes author in.
pub(crate) fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}
