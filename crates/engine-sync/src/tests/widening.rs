//! Widening an email scope's window (`docs/design/windows-and-widening.md`, §4): after its delta,
//! a pass asked for more than its cursor covers streams only what the wider window adds, keeps the
//! delta cursor, records the wider depth only once the band has ended, and resumes a killed band
//! from where it stopped. The yardstick is a cold pass under the wider window: a widening must
//! end in the same store.

use engine_core::{
    sync::{MailboxWindows, StampedCursor},
    time::CalendarDate,
};
use serde_json::Value;

use super::{widening_fake::Banded, *};

fn since(year: i32, month: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, 1).unwrap())
}

fn dated(id: &str, mailbox: &str, received: &str) -> Message {
    let mut message = message(id, mailbox, id);
    message.received_at = Some(received.parse().unwrap());
    message
}

/// Mail at chosen dates: two inside three months, two in the three before, one older still.
fn mail() -> Vec<Message> {
    vec![
        dated("m-now", "INBOX", "2026-06-10T09:00:00Z"),
        dated("m-floor", "INBOX", "2026-04-01T00:00:00Z"),
        dated("m-edge", "Sent", "2026-03-31T23:59:59Z"),
        dated("m-band", "INBOX", "2026-01-15T09:00:00Z"),
        dated("m-old", "INBOX", "2024-01-15T09:00:00Z"),
    ]
}

fn narrow() -> MailboxWindows {
    since(2026, 4).into()
}

fn wide() -> MailboxWindows {
    since(2026, 1).into()
}

/// Every commit the observer saw, as `(widening, fetched, total, tombstoned)`.
type Commits = Mutex<Vec<(bool, usize, Option<usize>, usize)>>;

async fn pass_seen(
    provider: &Banded,
    store: &SqliteStore<ManualClock>,
    window: MailboxWindows,
    commits: &Commits,
) -> super::super::MailSyncReport {
    let observer = |commit: &SyncCommit<'_>| {
        commits.lock().unwrap().push((
            commit.widening,
            commit.fetched,
            commit.total,
            commit.tombstoned,
        ));
    };
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 1).within(window),
        &observer,
    )
    .await
}

async fn pass(
    provider: &Banded,
    store: &SqliteStore<ManualClock>,
    window: MailboxWindows,
) -> super::super::MailSyncReport {
    pass_seen(provider, store, window, &Mutex::new(Vec::new())).await
}

fn scope() -> SyncScope {
    SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    }
}

async fn objects(store: &SqliteStore<ManualClock>) -> Vec<(ProviderKey, Value)> {
    let mut objects = store.scope_objects(&scope()).await.unwrap();
    objects.sort_by(|a, b| a.0.cmp(&b.0));
    objects
}

async fn record(store: &SqliteStore<ManualClock>) -> StampedCursor {
    StampedCursor::parse(
        store
            .load_sync_state(account(), &scope())
            .await
            .unwrap()
            .unwrap(),
    )
}

/// The store a cold pass under `window` ends in: the yardstick.
async fn cold(window: MailboxWindows) -> Vec<(ProviderKey, Value)> {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&Banded::new(mail()), &store, window).await.is_ok());
    objects(&store).await
}

async fn coverage(store: &SqliteStore<ManualClock>) -> SyncWindow {
    store.recipient_coverage(Some(account())).await.unwrap()[0].window
}

#[tokio::test]
async fn a_widening_fetches_only_the_band_keeps_the_cursor_and_ends_as_a_cold_pass_would() {
    let provider = Banded::new(mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    let held = record(&store).await;
    // A change on the server, so the delta has something to move the cursor past.
    provider
        .server
        .lock()
        .unwrap()
        .put(dated("m-new", "INBOX", "2026-06-20T09:00:00Z"));

    let report = pass(&provider, &store, wide()).await;

    assert!(report.is_ok(), "{report:?}");
    assert!(report.folders[0].reached_window);
    assert_eq!(
        *provider.band_fetched.lock().unwrap(),
        ["m-edge", "m-band"],
        "only the band was read: nothing held again, nothing past the new floor"
    );
    let ended = record(&store).await;
    assert_eq!(ended.covered(), &wide());
    assert_eq!(ended.target(), None);
    assert_eq!(
        ended.cursor().as_str(),
        "s6",
        "the delta's cursor, kept by the band"
    );
    assert_ne!(held.cursor(), ended.cursor());
    let mut expected = cold(wide()).await;
    let new = dated("m-new", "INBOX", "2026-06-20T09:00:00Z");
    expected.retain(|(key, _)| key != new.id.key());
    let mut got = objects(&store).await;
    got.retain(|(key, _)| key != new.id.key());
    assert_eq!(got, expected, "the same keys and states as a cold pass");
    assert_eq!(coverage(&store).await, since(2026, 1));
}

#[tokio::test]
async fn a_band_tombstones_nothing_and_reports_progress_against_its_own_total() {
    let provider = Banded::new(mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    // Gone from the server with no delta to say so: only a reconcile would tombstone it.
    provider
        .server
        .lock()
        .unwrap()
        .messages
        .retain(|(m, _)| m.id.key().as_str() != "m-now");

    let commits = Mutex::new(Vec::new());
    assert!(pass_seen(&provider, &store, wide(), &commits).await.is_ok());

    let band: Vec<_> = commits
        .lock()
        .unwrap()
        .iter()
        .filter(|(widening, ..)| *widening)
        .copied()
        .collect();
    assert_eq!(
        band,
        [(true, 1, Some(2), 0), (true, 2, Some(2), 0)],
        "one commit per band chunk, counted against the band's own total"
    );
    let keys: Vec<String> = objects(&store)
        .await
        .iter()
        .map(|(key, _)| key.as_str().to_owned())
        .collect();
    assert!(keys.contains(&"m-now".to_owned()), "the band tombstoned");
}

#[tokio::test]
async fn a_band_killed_mid_way_resumes_from_its_position_and_holds_the_depth_until_it_ends() {
    let provider = Banded::new(mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    *provider.fail_after.lock().unwrap() = Some(1);

    let killed = pass(&provider, &store, wide()).await;
    assert!(!killed.is_ok());
    assert!(!killed.folders[0].reached_window);
    let mid = record(&store).await;
    assert_eq!(
        mid.covered(),
        &narrow(),
        "claimed mail the band never fetched"
    );
    assert_eq!(mid.target(), Some(&wide()));
    assert_eq!(
        coverage(&store).await,
        since(2026, 4),
        "coverage moved before the band ended"
    );

    assert!(pass(&provider, &store, wide()).await.is_ok());
    assert_eq!(
        *provider.widen_starts.lock().unwrap(),
        [0, 1],
        "the band started over instead of continuing"
    );
    assert_eq!(*provider.band_fetched.lock().unwrap(), ["m-edge", "m-band"]);
    assert_eq!(objects(&store).await, cold(wide()).await);
    assert_eq!(record(&store).await.covered(), &wide());
    assert_eq!(coverage(&store).await, since(2026, 1));
}

#[tokio::test]
async fn a_target_changed_mid_band_restarts_the_band_from_the_covered_depth() {
    let provider = Banded::new(mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    *provider.fail_after.lock().unwrap() = Some(1);
    assert!(!pass(&provider, &store, wide()).await.is_ok());

    let everything = MailboxWindows::default();
    assert!(pass(&provider, &store, everything.clone()).await.is_ok());

    assert_eq!(
        *provider.widen_starts.lock().unwrap(),
        [0, 0],
        "a position recorded for another band was continued"
    );
    assert_eq!(objects(&store).await, cold(everything.clone()).await);
    assert_eq!(record(&store).await.covered(), &everything);
}

#[tokio::test]
async fn a_floor_that_moved_on_or_a_narrower_window_never_widens() {
    let provider = Banded::new(mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    assert!(pass(&provider, &store, since(2026, 5).into()).await.is_ok());
    assert!(pass(&provider, &store, since(2026, 6).into()).await.is_ok());

    assert!(provider.widen_starts.lock().unwrap().is_empty());
    assert_eq!(record(&store).await.covered(), &since(2026, 6).into());
}

#[tokio::test(start_paused = true)]
async fn a_cursor_cleared_mid_band_stops_the_band_at_its_next_commit() {
    const EVERY: Duration = Duration::from_secs(5);
    let provider = Banded {
        pace: Some(EVERY),
        ..Banded::new(mail())
    };
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());

    let host = async {
        // One band chunk in, the host clears the cursor.
        tokio::time::sleep(EVERY * 3 / 2).await;
        store.clear_scope_cursor(&scope()).await.unwrap();
    };
    let (report, ()) = tokio::join!(pass(&provider, &store, wide()), host);

    assert!(report.is_ok(), "{report:?}");
    assert!(!report.folders[0].reached_window);
    assert_eq!(
        store.load_sync_state(account(), &scope()).await.unwrap(),
        None,
        "the band wrote its cursor back over the clear"
    );
}

fn band_mail() -> Vec<Message> {
    vec![
        dated("m-now", "INBOX", "2026-06-10T09:00:00Z"),
        dated("m-a", "INBOX", "2026-03-20T09:00:00Z"),
        dated("m-b", "INBOX", "2026-02-10T09:00:00Z"),
    ]
}

#[tokio::test]
async fn a_narrower_pass_between_two_widenings_restarts_the_band_and_stores_what_it_dropped() {
    let provider = Banded::new(band_mail());
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    *provider.fail_after.lock().unwrap() = Some(1);
    assert!(!pass(&provider, &store, wide()).await.is_ok());
    // The host asks the narrower window again (no prune, no clear).
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    // An old message filed meanwhile: the narrower delta drops it by the window.
    provider
        .server
        .lock()
        .unwrap()
        .put(dated("m-filed", "INBOX", "2026-03-25T09:00:00Z"));
    assert!(pass(&provider, &store, narrow()).await.is_ok());
    // Back to the wider window: the same band, so the position is continued.
    assert!(pass(&provider, &store, wide()).await.is_ok());
    assert_eq!(
        *provider.widen_starts.lock().unwrap(),
        [0, 0],
        "a position recorded before a narrower pass was continued"
    );
    let keys: Vec<String> = objects(&store)
        .await
        .iter()
        .map(|(key, _)| key.as_str().to_owned())
        .collect();
    assert!(
        keys.contains(&"m-filed".to_owned()),
        "gap: m-filed never stored"
    );
}
