//! Shared setup for the gated `provider-google` live calendar suites.
//!
//! Its own module rather than a copy per suite: each needs the same token gate and the same
//! bound provider, and a second copy is a second thing to keep in step with the adapter.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use engine_core::ids::{AccountId, CalendarId};
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
    )
    .expect("client");
    // "primary" is Google's alias for the account's default calendar.
    GoogleCalendarProvider::new(client, CalendarId::try_from("primary").unwrap())
}
