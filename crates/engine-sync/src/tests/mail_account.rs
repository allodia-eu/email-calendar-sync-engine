//! The account pass: the fan-out it owns, the order it chooses, what it reports, and what it
//! tells an observer while it runs.
//!
//! These are the behaviours that only exist because the engine drives the folders itself. When a
//! host did it, none of them could be tested here at all — which is how a repair ended up in an
//! entrypoint the shipping client never called.

use super::*;

/// The account's folders, as one provider each — the IMAP shape.
fn folders(names: &[&str], log: &Arc<Mutex<Vec<MailboxId>>>) -> Vec<FakeMail> {
    let boxes: Vec<Mailbox> = names
        .iter()
        .map(|name| mailbox(name, name, (*name == "INBOX").then_some(MailboxRole::Inbox)))
        .collect();
    names
        .iter()
        .map(|name| {
            FakeMail::new(boxes.clone(), vec![message("m", name, "Hi")]).in_folder(name, log)
        })
        .collect()
}

/// Counts what an observer is told, which is all a host needs for "syncing, 5 of 10".
#[derive(Default)]
struct Watcher {
    started: Mutex<Vec<(usize, Option<MailboxId>)>>,
    finished: Mutex<Vec<(SyncScope, bool)>>,
    ended: Mutex<usize>,
}

impl SyncObserver for Watcher {
    fn committed(&self, _commit: &SyncCommit<'_>) {}

    fn account_sync_started(
        &self,
        _account: &AccountId,
        folders: usize,
        inbox: Option<&MailboxId>,
    ) {
        self.started.lock().unwrap().push((folders, inbox.cloned()));
    }

    fn folder_sync_finished(&self, _account: &AccountId, scope: &SyncScope, synced: bool) {
        self.finished.lock().unwrap().push((scope.clone(), synced));
    }

    fn account_sync_finished(&self, _account: &AccountId) {
        *self.ended.lock().unwrap() += 1;
    }
}

#[tokio::test]
async fn a_pass_syncs_every_folder_and_reports_one_outcome_each() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let providers = folders(&["INBOX", "Archive", "Projects"], &log);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync_mail(
        &providers,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;

    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.folders.len(), 3, "one entry per folder, always");
    assert_eq!(report.folders_synced(), 3);
    assert_eq!(report.busy_scopes(), 0);
    // Each folder is its own scope, so the report can name which one did what — the thing a
    // single `Result` for the whole account could never say.
    let scopes: BTreeSet<String> = report
        .folders
        .iter()
        .map(|f| format!("{:?}", f.scope))
        .collect();
    assert_eq!(scopes.len(), 3, "three distinct scopes");
}

#[tokio::test]
async fn the_observer_is_told_the_folder_count_and_each_folder_as_it_lands() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let providers = folders(&["INBOX", "Archive"], &log);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let watcher = Watcher::default();

    sync_mail(
        &providers,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &watcher,
    )
    .await;

    // The denominator and the Inbox arrive together, once, after the folder list — before it
    // there is nothing to divide by and no role to look up. A host filing streaming rows by
    // folder needs the Inbox now, not when the pass ends.
    let started = watcher.started.lock().unwrap().clone();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, 2, "both folders");
    assert_eq!(
        started[0].1.as_ref().map(MailboxId::as_str),
        Some("INBOX"),
        "and the account's Inbox, resolved from the folder list this pass just synced"
    );
    let finished = watcher.finished.lock().unwrap();
    assert_eq!(finished.len(), 2, "one per folder");
    assert!(finished.iter().all(|(_, synced)| *synced));
    assert_eq!(*watcher.ended.lock().unwrap(), 1, "and exactly one end");
}

#[tokio::test]
async fn a_pass_with_no_providers_still_opens_and_closes_its_progress() {
    // An account with nothing connected is not an error, and a host that raised an indicator on
    // `started` must still be told to clear it — otherwise the hint sticks until a restart.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let watcher = Watcher::default();

    let report = sync_mail(
        &[] as &[FakeMail],
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &watcher,
    )
    .await;

    assert!(report.is_ok());
    assert!(report.folders.is_empty());
    assert_eq!(*watcher.started.lock().unwrap(), vec![(0, None)]);
    assert_eq!(*watcher.ended.lock().unwrap(), 1);
}
