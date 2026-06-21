//! Shared helpers for the gated live CalDAV suites (Stalwart and SabreDAV).
//!
//! Both servers exercise the **same** write round-trip, so the client is proven not
//! over-fit to one implementation — the CardDAV-style insurance the read tests
//! already give discovery + sync-token, now extended to conditional `PUT`/`DELETE`.

use engine_core::calendar::Event;
use engine_core::ids::{AccountId, Uid};
use engine_core::raw::RawIcal;
use engine_core::sync::SyncUpdate;
use engine_provider::{EventDeletion, EventWrite, Provider};
use provider_caldav::CalDavProvider;
use tokio::sync::{Mutex, MutexGuard};

/// Serializes the live tests within one binary: the write round-trip transiently
/// adds an event to the shared calendar, so it must not overlap the sync-loop
/// test's exact-count assertion. A `tokio::sync::Mutex` (not `std`), so the guard
/// is safely held across the `.await`s of the whole test body; it carries no poison
/// state, and the round-trip pre-cleans residue so a failed run never wedges later
/// ones.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// Acquires the per-binary live-test serialization guard for the test's duration.
pub(crate) async fn serial_guard() -> MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// The UID of the throwaway event the round-trip creates, updates, and deletes. A
/// distinct, recognizable value so it never collides with the seed fixtures.
const WRITE_TEST_UID: &str = "caldav-write-roundtrip@test.local";

/// One iCalendar body for the write test, with `title` as the `SUMMARY` and
/// `sequence` as the `SEQUENCE` (bumped on the edit, per iTIP).
fn body(title: &str, sequence: u32) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-write-test//EN\r\n\
         BEGIN:VEVENT\r\nUID:{WRITE_TEST_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260601T100000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260601T110000\r\n\
         SEQUENCE:{sequence}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// The current snapshot of the bound collection's events (a full re-read).
async fn snapshot(provider: &CalDavProvider, account: &AccountId) -> Vec<Event> {
    let synced = provider
        .sync_events(account, None)
        .await
        .expect("sync_events snapshot");
    match synced.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    }
}

/// The written test event in the current snapshot, or `None` once deleted.
async fn fetch(provider: &CalDavProvider, account: &AccountId) -> Option<Event> {
    snapshot(provider, account)
        .await
        .into_iter()
        .find(|event| event.uid.as_str() == WRITE_TEST_UID)
}

/// Drives the full CalDAV write lifecycle against a live server: create → verify →
/// update (`If-Match`) → verify → delete (`If-Match`) → verify gone.
///
/// The create `PUT`s to a minted `<collection>/<uid>.ics` href; **update and delete
/// target the server-returned `EventId`** (the resource's canonical href from sync),
/// mirroring how a real host works and staying robust if a server canonicalizes the
/// resource name (e.g. percent-encoding) differently from the minted href. The
/// authoritative `getetag` for each `If-Match` is read from a fresh sync rather than
/// trusting the `PUT` response header, so it is robust whether or not the server
/// echoes the new ETag — it asserts only on harness-controlled content (titles,
/// presence), never on the server-assigned ETag/href values.
pub(crate) async fn write_round_trip(provider: &CalDavProvider, account: &AccountId) {
    assert!(
        provider.capabilities().calendar_writes(),
        "the CalDAV provider advertises calendar writes"
    );
    let create_href = provider
        .event_href(&Uid::new(WRITE_TEST_UID).unwrap())
        .expect("mint event href");

    // Pre-clean any residue from a prior interrupted run, so the create is a true
    // create (If-None-Match: *).
    let _ = provider
        .delete_event(account, &EventDeletion::unconditional(create_href.clone()))
        .await;

    // ---- Create. ----
    let created = provider
        .put_event(
            account,
            &EventWrite::create(
                create_href,
                Uid::new(WRITE_TEST_UID).unwrap(),
                RawIcal::new(body("Live write test", 0)),
            ),
        )
        .await
        .expect("create event");
    assert_eq!(created.uid.as_str(), WRITE_TEST_UID);

    let made = fetch(provider, account)
        .await
        .expect("created event is present after sync");
    assert_eq!(made.title, "Live write test");
    // The server's canonical href for the resource — what subsequent writes target.
    let server_href = made.id.clone();
    let etag_v1 = made
        .revisions
        .etag
        .clone()
        .expect("server supplies a getetag");

    // ---- Update, guarded by If-Match on the current ETag. ----
    provider
        .put_event(
            account,
            &EventWrite::update(
                server_href.clone(),
                Uid::new(WRITE_TEST_UID).unwrap(),
                RawIcal::new(body("Live write test (edited)", 1)),
                etag_v1,
            ),
        )
        .await
        .expect("update event");

    let edited = fetch(provider, account)
        .await
        .expect("edited event is present after sync");
    assert_eq!(edited.title, "Live write test (edited)");
    let etag_v2 = edited
        .revisions
        .etag
        .clone()
        .expect("server supplies a getetag");

    // ---- Delete, guarded by If-Match on the post-update ETag. ----
    provider
        .delete_event(account, &EventDeletion::if_match(server_href, etag_v2))
        .await
        .expect("delete event");

    assert!(
        fetch(provider, account).await.is_none(),
        "the event is gone from the collection after the delete"
    );
}
