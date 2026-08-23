//! Shared helpers for the gated live CalDAV suites (Stalwart and SabreDAV).
//!
//! Both servers run the **same** write scenarios ([`write`]), so the client is proven
//! not over-fit to one implementation — the insurance the read tests already give
//! discovery + sync-token, extended to conditional `PUT`/`DELETE` and to the
//! structural patcher.
//!
//! The two servers are *not* interchangeable as evidence, and the difference is the
//! reason the preservation check is written the way it is:
//!
//! - **SabreDAV stores the bytes verbatim** — what you `PUT` is what you `GET`.
//! - **Stalwart reserializes** — it keeps every property but re-folds content lines and reorders
//!   `RRULE` parts, so the document it hands back is *not* the one you sent.
//!
//! So a preservation assertion can never compare our bytes with the server's. It
//! compares the **server's own copy before the patch** with the **server's own copy
//! after it**: whatever the server does to the formatting it does to both, and anything
//! it *drops* shows up as a missing line. See [`write::patched_update_preserves_the_document`].

pub(crate) mod imip;
pub(crate) mod reconcile;
pub(crate) mod recurrence;
pub(crate) mod write;

use engine_core::{
    calendar::Event,
    ids::{AccountId, Uid},
    raw::RawIcal,
    sync::SyncUpdate,
    version::ETag,
};
use engine_provider::{EventDeletion, Provider};
use provider_caldav::CalDavProvider;
use tokio::sync::{Mutex, MutexGuard};

/// Serializes the live tests within one binary: every write scenario transiently adds
/// an event to the shared calendar, so none may overlap the sync-loop test's exact-count
/// assertion, nor each other's. A `tokio::sync::Mutex` (not `std`), so the guard is
/// safely held across the `.await`s of a whole test body; it carries no poison state,
/// and each scenario pre-cleans its own residue so a failed run never wedges later ones.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// Acquires the per-binary live-test serialization guard for the test's duration.
pub(crate) async fn serial_guard() -> MutexGuard<'static, ()> {
    SERIAL.lock().await
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

/// The event with `uid` as the **server** currently stores it, or `None` once deleted.
///
/// Every scenario reads its event back through this rather than trusting what it sent:
/// the whole question these tests exist to answer is what the *server* did with it.
pub(crate) async fn fetch(
    provider: &CalDavProvider,
    account: &AccountId,
    uid: &str,
) -> Option<Event> {
    snapshot(provider, account)
        .await
        .into_iter()
        .find(|event| event.uid.as_str() == uid)
}

/// Reads the event back, failing with a message naming the scenario's event.
async fn require(provider: &CalDavProvider, account: &AccountId, uid: &str) -> Event {
    fetch(provider, account, uid)
        .await
        .unwrap_or_else(|| panic!("event {uid} is present on the server"))
}

/// The server's stored iCalendar for an event — the bytes it hands back, not the ones
/// we sent (they differ: Stalwart reserializes).
pub(crate) fn server_ical(event: &Event) -> RawIcal {
    event
        .raw_ical
        .clone()
        .expect("the read path preserves the raw iCalendar")
}

/// The event's current `getetag`, for an `If-Match`.
fn server_etag(event: &Event) -> ETag {
    event
        .revisions
        .etag
        .clone()
        .expect("the server supplies a getetag")
}

/// Removes any residue of `uid` from a prior interrupted run, so a scenario's create is
/// a true create (`If-None-Match: *`). Unconditional: the residue's ETag is unknown.
pub(crate) async fn pre_clean(provider: &CalDavProvider, account: &AccountId, uid: &Uid) {
    let href = provider.event_href(uid).expect("mint event href");
    let _ = provider
        .delete_event(account, &EventDeletion::unconditional(href, uid.clone()))
        .await;
}

/// The document's **logical** (unfolded) content lines, minus the ones whose property
/// name is in `struck`.
///
/// Unfolding first means a server that re-folds at different octets (Stalwart does) is
/// not mistaken for one that changed the content — the question is what a document
/// *says*, not where its line breaks land. This is the integration-test counterpart of
/// the patcher's own structural assertion: strike what the patch was allowed to touch,
/// and everything remaining must be identical.
fn lines_without(ical: &str, struck: &[&str]) -> Vec<String> {
    let mut logical: Vec<String> = Vec::new();
    for line in ical.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        // RFC 5545 §3.1: a line beginning with a space or tab continues the previous one.
        if let Some(continuation) = line.strip_prefix([' ', '\t'])
            && let Some(previous) = logical.last_mut()
        {
            previous.push_str(continuation);
            continue;
        }
        logical.push(line.to_owned());
    }
    logical.retain(|line| {
        let name = line.split([';', ':']).next().unwrap_or(line);
        !struck
            .iter()
            .any(|target| name.eq_ignore_ascii_case(target))
    });
    logical
}
