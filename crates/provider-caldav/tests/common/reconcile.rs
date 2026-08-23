//! The live **read-your-writes** scenario (issue #65), run against every real server the
//! harness offers.
//!
//! This is the one claim the offline suite structurally cannot make. The fix for #65 rests
//! on a single assumption about a *server*: that a `sync-collection` delta, taken with the
//! token held from **before** our own `PUT`, re-delivers the resource **we** just wrote —
//! with the server's `calendar-data` inline and the server's new `ETag`. An offline fake
//! replays canned bytes and would "confirm" that no matter what. Only a server can.
//!
//! So this drives the whole chain a host drives — sync → write → reconcile → **re-read from
//! the store** → write again — and asserts what the store holds at each step. The last leg
//! is the point: the second edit's `If-Match` is built from the store, and if the reconcile
//! had not refreshed it, the server would refuse it with the `412` this issue is named for.

use engine_core::{
    calendar::Event,
    ids::{AccountId, Uid},
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDeletion, EventDraft, EventPatch, PatchTarget, Provider};
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{
    delete_calendar_event, patch_calendar_event, reconcile_calendar_events, sync_calendar,
};
use provider_caldav::CalDavProvider;
use store_sqlite::SqliteStore;

use super::pre_clean;

const UID: &str = "caldav-read-your-writes@test.local";

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn horizon() -> Horizon {
    Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2026-12-31T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

fn worker() -> WorkerId {
    WorkerId::new("live-reconcile")
}

fn ttl() -> core::time::Duration {
    core::time::Duration::from_mins(1)
}

/// The event with [`UID`] as the **store** currently holds it.
async fn stored(
    store: &SqliteStore<ManualClock>,
    provider: &CalDavProvider,
    account: &AccountId,
) -> Option<Event> {
    let scope = provider.event_scope(account);
    for key in store.object_keys(&scope).await.expect("object keys") {
        let payload = store
            .object_payload(&scope, &key)
            .await
            .expect("object payload")
            .expect("a key the store just listed");
        let event: Event = serde_json::from_value(payload).expect("stored event");
        if event.uid.as_str() == UID {
            return Some(event);
        }
    }
    None
}

/// Write → reconcile → re-read → write again, all through the store, against a real server.
pub(crate) async fn read_your_writes(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new(UID).unwrap();
    pre_clean(provider, account, &uid).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap()))
            .expect("store");
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    // A first sync, so the event scope holds a real sync-token — the cursor every
    // reconcile below takes its delta from.
    sync_calendar(
        provider,
        &store,
        account,
        worker(),
        ttl(),
        horizon(),
        &host_zone,
    )
    .await
    .expect("first sync");

    // ---- Create, and reconcile it into the store. ----
    provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Read your writes",
                amsterdam("2026-06-05T10:00:00"),
                amsterdam("2026-06-05T11:00:00"),
                stamp(),
            ),
        )
        .await
        .expect("create the event");
    reconcile_calendar_events(provider, &store, account, worker(), ttl())
        .await
        .expect("reconcile the create");

    let created = stored(&store, provider, account)
        .await
        .expect("the create is in the store after the reconcile, with no further sync");
    assert_eq!(created.title, "Read your writes");
    assert!(
        created.raw_ical.is_some() && created.revisions.etag.is_some(),
        "the delta carried the server's calendar-data and its ETag inline — a bare receipt \
         carries neither"
    );

    // ---- Edit it, through the store's copy, and reconcile. ----
    let first = patch_calendar_event(
        provider,
        &store,
        account,
        worker(),
        ttl(),
        "live:ryw:edit-1",
        &created,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Read your writes (edited once)"),
    )
    .await
    .expect("the first edit lands");
    reconcile_calendar_events(provider, &store, account, worker(), ttl())
        .await
        .expect("reconcile the first edit");

    let reread = stored(&store, provider, account)
        .await
        .expect("still stored");
    assert_eq!(reread.title, "Read your writes (edited once)");
    assert_eq!(
        reread.revisions.etag, first.revisions.etag,
        "the store now holds the revision the WRITE reported — the pre-write one is gone, \
         which is the whole of issue #65"
    );

    // ---- The money shot: edit again from the store's copy. ----
    //
    // The guard for this write is the ETag the store holds. Before the reconcile that was
    // the *superseded* one, and this `If-Match` would come back `412 Precondition Failed`
    // on a write that should plainly have succeeded.
    patch_calendar_event(
        provider,
        &store,
        account,
        worker(),
        ttl(),
        "live:ryw:edit-2",
        &reread,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Read your writes (edited twice)"),
    )
    .await
    .expect("a second edit built from the STORE must not be refused on a stale If-Match");
    reconcile_calendar_events(provider, &store, account, worker(), ttl())
        .await
        .expect("reconcile the second edit");
    let twice = stored(&store, provider, account)
        .await
        .expect("still stored");
    assert_eq!(twice.title, "Read your writes (edited twice)");

    // ---- Delete: the reconcile must tombstone the local row, not merely the remote one. ----
    delete_calendar_event(
        provider,
        &store,
        account,
        worker(),
        ttl(),
        "live:ryw:delete",
        None,
        &EventDeletion::of(&twice),
    )
    .await
    .expect("the delete lands");
    reconcile_calendar_events(provider, &store, account, worker(), ttl())
        .await
        .expect("reconcile the delete");
    assert!(
        stored(&store, provider, account).await.is_none(),
        "the sync-collection delta reported the resource as removed (404), so the store \
         tombstoned it — the event does not linger locally until the next full sync"
    );
}
