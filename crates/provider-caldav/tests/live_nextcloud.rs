//! Gated live integration: the CalDAV connect, sync loop and write scenarios against the
//! **Nextcloud** harness (`docker/nextcloud/`), the third server beside Stalwart and SabreDAV.
//!
//! Nextcloud is SabreDAV underneath, with its own authentication in front, its own principal
//! layout (`/remote.php/dav/principals/users/<user>/`), several collections in one home (a
//! default calendar, a generated birthday calendar, an inbox and an outbox) and RFC 6638
//! scheduling switched on. So the same seed assertions as `live_sabredav.rs` here prove the
//! client follows a real deployment's discovery and binds the calendar it was asked for, and
//! the scheduling check is the positive half of SabreDAV's negative one. Skips unless
//! `NEXTCLOUD_HTTP_ADDR` is set, so the offline `cargo test --workspace` stays green.

use core::time::Duration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, ProviderKey},
    sync::SyncScope,
    time::TimeZoneId,
};
use engine_provider::{CalendarUserAddresses, CalendarWrites, Provider};
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use serde::de::DeserializeOwned;
use store_sqlite::SqliteStore;

mod common;

/// The seeded calendar: the account's home also holds `personal` and the birthday calendar.
const CALENDAR: &str = "harness";

/// Reads the Nextcloud harness coordinates, or `None` to skip (offline gate).
fn harness() -> Option<(String, String, String)> {
    let addr = std::env::var("NEXTCLOUD_HTTP_ADDR").ok()?;
    let user = std::env::var("NEXTCLOUD_USER").unwrap_or_else(|_| "alice".to_owned());
    let pass = std::env::var("NEXTCLOUD_PASS").unwrap_or_else(|_| "nextcloud-alice-pw".to_owned());
    Some((addr, user, pass))
}

/// Connects from the server's root, as a host given only the server name does, retrying
/// briefly so a just-started container is tolerated.
async fn connect(addr: &str, user: &str, pass: &str) -> CalDavProvider {
    let config = CalDavConfig::new(
        format!("http://{addr}"),
        Credentials::Basic {
            username: user.to_owned(),
            password: pass.to_owned(),
        },
        engine_http::test_server::user_agent(),
    )
    .with_calendar(CALENDAR);
    let mut last_err = None;
    for _ in 0..15 {
        match CalDavProvider::connect(config.clone()).await {
            Ok(provider) => return provider,
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to the Nextcloud harness: {last_err:?}");
}

/// Connects to the Nextcloud harness, or `None` to skip (offline gate).
async fn provider(test: &str) -> Option<(CalDavProvider, AccountId)> {
    let Some((addr, user, pass)) = harness() else {
        eprintln!("skipping {test}: NEXTCLOUD_HTTP_ADDR unset");
        return None;
    };
    let provider = connect(&addr, &user, &pass).await;
    Some((provider, AccountId::try_from("nextcloud-live").unwrap()))
}

/// The well-known redirect leads to `/remote.php/dav/`, which challenges, and the principal
/// it then names is the one a host stores. Its address set lists the account's `mailto:`
/// beside the principal's own path, which is not an email address and is left out.
#[tokio::test]
async fn nextcloud_is_discovered_from_the_server_name() {
    let Some((addr, user, _)) = harness() else {
        eprintln!(
            "skipping nextcloud_is_discovered_from_the_server_name: NEXTCLOUD_HTTP_ADDR unset"
        );
        return;
    };
    let Some((provider, account)) = provider("nextcloud_is_discovered_from_the_server_name").await
    else {
        return;
    };

    assert_eq!(
        provider.principal_url(),
        Some(format!("http://{addr}/remote.php/dav/principals/users/{user}/").as_str())
    );
    assert_eq!(
        provider
            .calendar_user_addresses(&account)
            .await
            .expect("the principal answers"),
        CalendarUserAddresses::Known(vec!["alice@test.local".to_owned()])
    );
}

async fn load<T: DeserializeOwned>(
    store: &SqliteStore<ManualClock>,
    scope: &SyncScope,
    key: &ProviderKey,
) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

#[tokio::test]
async fn nextcloud_calendar_sync_loop() {
    let Some((provider, account)) = provider("nextcloud_calendar_sync_loop").await else {
        return;
    };
    // Serialize with the writes, which add to the same calendar.
    let _serial = common::serial_guard().await;

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");
    assert!(
        report.calendars.upserted >= 3,
        "every calendar in the home is listed, not only the bound one"
    );

    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "six seed calendar resources stored");

    let mut events = Vec::new();
    for key in &event_keys {
        events.push(load::<Event>(&store, &event_scope, key).await);
    }
    let by_uid = |uid: &str| events.iter().find(|e| e.uid.as_str() == uid).unwrap();
    assert_eq!(by_uid("meeting-2003@test.local").participants.len(), 3);
    assert_eq!(by_uid("virtual-2004@test.local").virtual_locations.len(), 1);
    assert!(by_uid("allday-2005@test.local").is_all_day());
    assert!(by_uid("floating-2006@test.local").start.is_floating());

    let weekly = by_uid("weekly-2002@test.local");
    assert!(weekly.is_recurring());
    let mut total = 0;
    for key in &event_keys {
        total += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    assert_eq!(
        store
            .index_row_counts(&event_scope, weekly.id.key())
            .await
            .unwrap()
            .occurrences,
        7
    );
    assert_eq!(total, 12);

    let second = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("second sync_calendar");
    assert_eq!(
        second.events.applied.upserted, 0,
        "no event changes on a re-sync"
    );
    assert_eq!(
        second.events.applied.tombstoned, 0,
        "nothing tombstoned on a re-sync"
    );
}

/// Nextcloud loads SabreDAV's scheduling plugin, so its `OPTIONS` reports
/// `calendar-auto-schedule`: the positive answer to the question `sabredav_reports_no_scheduling`
/// answers negatively, on the same server software.
#[tokio::test]
async fn nextcloud_reports_scheduling() {
    let Some((provider, _)) = provider("nextcloud_reports_scheduling").await else {
        return;
    };
    common::imip::scheduling_is_discovered_from_the_server(&provider, true);
}

#[tokio::test]
async fn nextcloud_write_round_trip() {
    let Some((provider, account)) = provider("nextcloud_write_round_trip").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::round_trip(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_patched_update_preserves_the_document() {
    let Some((provider, account)) =
        provider("nextcloud_patched_update_preserves_the_document").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::patched_update_preserves_the_document(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_stale_if_match_is_a_conflict() {
    let Some((provider, account)) = provider("nextcloud_stale_if_match_is_a_conflict").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::stale_if_match_is_a_conflict(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_instance_override_split_is_accepted() {
    let Some((provider, account)) = provider("nextcloud_instance_override_split_is_accepted").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::instance_override_split_is_accepted(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_create_carries_the_recurrence_rule() {
    let Some((provider, account)) = provider("nextcloud_create_carries_the_recurrence_rule").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::create_carries_the_rule(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_recurrence_until_is_written_in_utc() {
    let Some((provider, account)) = provider("nextcloud_recurrence_until_is_written_in_utc").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::an_until_is_written_in_utc(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_a_rule_can_be_changed_and_removed() {
    let Some((provider, account)) = provider("nextcloud_a_rule_can_be_changed_and_removed").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::a_rule_can_be_changed_and_removed(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_an_occurrence_can_be_removed() {
    let Some((provider, account)) = provider("nextcloud_an_occurrence_can_be_removed").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::an_occurrence_can_be_removed(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_storing_an_invitation_is_a_guarded_create() {
    let Some((provider, account)) =
        provider("nextcloud_storing_an_invitation_is_a_guarded_create").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::imip::storing_an_invitation_is_a_guarded_create(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_write_reconciles_the_store() {
    let Some((provider, account)) = provider("nextcloud_write_reconciles_the_store").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::reconcile::read_your_writes(&provider, &account).await;
}

#[tokio::test]
async fn nextcloud_override_survival_is_what_the_adapter_advertises() {
    let Some((provider, account)) =
        provider("nextcloud_override_survival_is_what_the_adapter_advertises").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::survival::survival_is_what_the_adapter_advertises(&provider, &account).await;
}
