//! What every harness proves about an account with more than one calendar: one provider syncs
//! them all, each under a scope of its own, and a re-sync with nothing changed asks none of
//! them anything.

use engine_core::{calendar::Event, ids::AccountId};
use engine_provider::Provider;
use engine_store::{ManualClock, StoreRead};
use engine_sync::CalendarSyncReport;
use provider_caldav::CalDavProvider;
use store_sqlite::SqliteStore;

use super::calendars;

/// The second calendar's events, as the shared seed puts them in `work`.
const WORK_UIDS: [&str; 2] = ["work-2101@test.local", "work-2102@test.local"];

/// The seeded `work` calendar synced beside the other one, under a scope of its own that
/// holds its two events and nothing else.
pub(crate) async fn the_second_calendar_syncs_under_its_own_scope(
    provider: &CalDavProvider,
    store: &SqliteStore<ManualClock>,
    account: &AccountId,
    report: &CalendarSyncReport,
) {
    assert_eq!(report.failures().count(), 0, "every calendar synced");
    assert!(
        report.scopes.len() >= 2,
        "one scope per calendar, at least the two the seed holds: {}",
        report.scopes.len()
    );
    let work = calendars(provider, account)
        .await
        .into_iter()
        .find(|calendar| {
            calendar
                .id
                .as_str()
                .trim_end_matches('/')
                .ends_with("/work")
        })
        .expect("the seed's second calendar is listed");
    let scope = provider.event_scope(account, &work.id);
    let mut uids = Vec::new();
    for key in store.object_keys(&scope).await.unwrap() {
        let payload = store.object_payload(&scope, &key).await.unwrap().unwrap();
        let event: Event = serde_json::from_value(payload).unwrap();
        assert!(
            event.calendars.contains(&work.id),
            "filed under the calendar it came from"
        );
        uids.push(event.uid.as_str().to_owned());
    }
    uids.sort();
    assert_eq!(
        uids, WORK_UIDS,
        "the work calendar holds its own two events, and only those"
    );
}

/// A re-sync with nothing changed on the server: every calendar's tag is as recorded, so no
/// calendar is asked for its events.
pub(crate) fn a_quiet_resync_asks_no_calendar(report: &CalendarSyncReport) {
    assert_eq!(report.failures().count(), 0);
    assert_eq!(
        report.unchanged(),
        report.scopes.len(),
        "every calendar's listing tag matched the one its last sync recorded"
    );
    assert_eq!(report.events.applied.upserted, 0);
    assert_eq!(report.events.applied.tombstoned, 0);
}
