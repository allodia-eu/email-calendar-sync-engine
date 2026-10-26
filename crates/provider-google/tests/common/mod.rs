//! Shared setup for the gated `provider-google` live calendar suites.
//!
//! Its own module rather than a copy per suite: each needs the same token gate and the same
//! bound provider, and a second copy is a second thing to keep in step with the adapter.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use engine_core::{
    ids::{AccountId, CalendarId},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_provider::CalendarWrites;
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

pub(crate) fn calendar_provider(token: String) -> GoogleCalendarProvider {
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
