//! Shared setup for the gated JMAP live suites against the Stalwart harness.
//!
//! Extracted so the write scenarios and the recurrence scenarios can each live in their own
//! file under the 500-line cap, rather than one file growing past it. Mirrors
//! `provider-caldav/tests/common/`.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId},
    sync::SyncUpdate,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDeletion, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

pub(crate) async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect")
}

pub(crate) fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

pub(crate) fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Every event the account currently holds.
pub(crate) async fn all_events(provider: &JmapProvider) -> Vec<Event> {
    let events = provider.sync_events(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
        panic!("expected a snapshot");
    };
    objects
}

/// The event with `uid`, if the server still holds one.
pub(crate) async fn fetch(provider: &JmapProvider, uid: &str) -> Option<Event> {
    all_events(provider)
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == uid)
}

pub(crate) async fn require(provider: &JmapProvider, uid: &str) -> Event {
    fetch(provider, uid)
        .await
        .unwrap_or_else(|| panic!("event {uid} is present on the server"))
}

/// The account's first calendar — where a throwaway event lands.
pub(crate) async fn calendar(provider: &JmapProvider) -> CalendarId {
    let calendars = provider.sync_calendars(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = calendars.update else {
        panic!("expected a snapshot");
    };
    objects
        .into_iter()
        .next()
        .expect("the seeded account has a calendar")
        .id
}

/// Removes any residue of `uid` from a prior interrupted run.
pub(crate) async fn pre_clean(provider: &JmapProvider, uid: &str) {
    if let Some(stale) = fetch(provider, uid).await {
        provider
            .delete_event(&account(), &EventDeletion::of(&stale))
            .await
            .expect("clean up a prior run's event");
    }
}

/// Starts the harness, or `None` when the gate env var is unset.
pub(crate) async fn setup(name: &str) -> Option<JmapProvider> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {name}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    Some(connect(&harness).await)
}
