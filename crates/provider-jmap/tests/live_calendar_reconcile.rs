//! The live **read-your-writes** check for JMAP (issue #65): does a `CalendarEvent/changes`
//! delta, taken from the state held **before** our own `CalendarEvent/set`, re-deliver the
//! event *we* just wrote?
//!
//! That single question is the load-bearing assumption of the post-write reconcile, and no
//! offline test can answer it: the `FakeExecutor` replays canned bytes and would happily
//! "confirm" any delta at all. A server that reported only *other* clients' changes — or
//! that did not advance `state` for our own write — would leave the store holding the
//! pre-write event forever, and the whole fix would be a no-op nobody noticed.
//!
//! It is the JMAP counterpart of `provider-caldav`'s `common::reconcile`, minus the `ETag`
//! chain: a `CalendarEvent` carries no per-object revision, so there is no stale-guard
//! footgun on this transport (`live_calendar_write::a_stale_edit_is_not_refused`). What is
//! left — and what this pins — is that the store's copy still comes from the **server**.

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId, ProviderKey, Uid},
    sync::{SyncState, SyncUpdate},
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

const UID: &str = "jmap-read-your-writes@test.local";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// A full snapshot, and the state it was taken at — the cursor a first sync persists.
async fn snapshot(provider: &JmapProvider) -> (Vec<Event>, SyncState) {
    let synced = provider
        .sync_events(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = synced.update else {
        panic!("expected a snapshot with no cursor");
    };
    (objects, synced.next_cursor)
}

/// The delta since `cursor`: what changed, what was destroyed, and the new cursor.
async fn delta(
    provider: &JmapProvider,
    cursor: &SyncState,
) -> (Vec<Event>, Vec<ProviderKey>, SyncState) {
    let synced = provider
        .sync_events(&account(), Some(cursor))
        .await
        .expect("delta");
    let SyncUpdate::Delta {
        changed, removed, ..
    } = synced.update
    else {
        panic!("a cursored sync must be a delta, not a re-snapshot");
    };
    (changed, removed, synced.next_cursor)
}

async fn calendar(provider: &JmapProvider) -> CalendarId {
    let synced = provider
        .sync_calendars(&account(), None)
        .await
        .expect("calendars");
    let SyncUpdate::Snapshot { objects, .. } = synced.update else {
        panic!("expected a snapshot");
    };
    objects
        .into_iter()
        .next()
        .expect("the seeded account has a calendar")
        .id
}

/// A write's own change comes back on the next delta — create, update, and destroy alike.
#[tokio::test]
async fn a_delta_redelivers_our_own_write() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_delta_redelivers_our_own_write: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    let uid = Uid::new(UID).unwrap();
    // Clean any residue of an interrupted run.
    let (events, _) = snapshot(&provider).await;
    if let Some(stale) = events.iter().find(|e| e.uid.as_str() == UID) {
        provider
            .delete_event(&account(), &EventDeletion::of(stale))
            .await
            .expect("clean up");
    }

    // The cursor a host holds *before* it writes — exactly what the reconcile takes its
    // delta from.
    let (_, before) = snapshot(&provider).await;

    // ---- Create. ----
    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                uid.clone(),
                "Read your writes",
                amsterdam("2026-06-06T10:00:00"),
                amsterdam("2026-06-06T11:00:00"),
                stamp(),
            ),
        )
        .await
        .expect("create");

    let (changed, _, after_create) = delta(&provider, &before).await;
    let delivered = changed
        .iter()
        .find(|e| e.uid.as_str() == UID)
        .expect("the delta re-delivers the event WE created — the premise of the reconcile");
    assert_eq!(
        delivered.id, created.event,
        "under the id the server assigned, which the receipt is the only other place to learn"
    );
    assert_eq!(delivered.title, "Read your writes");
    assert_ne!(
        after_create.as_str(),
        before.as_str(),
        "and the state advanced, so the next pass does not re-deliver it forever"
    );

    // ---- Update. ----
    provider
        .patch_event(
            &account(),
            delivered,
            &EventEdit::new(
                delivered,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Read your writes (edited)"),
            ),
        )
        .await
        .expect("patch");

    let (changed, _, after_edit) = delta(&provider, &after_create).await;
    let edited = changed
        .iter()
        .find(|e| e.uid.as_str() == UID)
        .expect("the delta re-delivers our own update");
    assert_eq!(
        edited.title, "Read your writes (edited)",
        "and it carries the server's copy of it — the store's event comes from the server, \
         never from the object we sent"
    );

    // ---- Destroy: the delta must report it as removed, so the store tombstones it. ----
    provider
        .delete_event(&account(), &EventDeletion::of(edited))
        .await
        .expect("destroy");

    let (_, removed, _) = delta(&provider, &after_edit).await;
    assert!(
        removed.contains(edited.id.key()),
        "our own destroy comes back as a removed id, which is what tombstones the local row"
    );
}
