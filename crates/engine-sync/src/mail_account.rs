//! One account's mail sync, folder fan-out included.
//!
//! **This is the only entrypoint for syncing an account's mail, and that is the point.** It used
//! to be a choice of four — a whole-account convenience, a streaming variant, and the two halves a
//! host drove itself — and the shipping client used only the two halves. Anything account-level
//! therefore had to be written into two functions that never call each other, and work put in the
//! convenience ran in the tests and nowhere else. The engine owns the fan-out now, so there is one
//! place for it and the tests drive what ships.
//!
//! Owning the fan-out is also what makes three things possible that a host driving it cannot do:
//! the account-level store steps run **once** instead of once per folder, the Inbox goes **first**
//! so the list the user is looking at fills before the archive, and the concurrency is **bounded**
//! rather than "every folder at once".

use core::time::Duration;
use std::{collections::BTreeSet, time::Instant};

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole},
    sync::{ObjectKind, SyncScope},
};
use engine_provider::Provider;
use engine_store::{ContactStore, LeaseRequest, Store, StoreRead, SyncApplied, WorkerId};
use futures_util::{StreamExt, stream};

use crate::{
    MailboxScope, StreamTuning, SyncError, SyncObserver, recipients, run_scope,
    stream::{FolderPass, stream_email},
    threading::repair_thread_index_if_damaged,
};

/// How many of an account's folders sync at once.
///
/// Not a network figure: an IMAP provider dials on construction, so by the time the engine is
/// handed one the sockets already exist and bounding this cannot close them (that is what the
/// connection pool is for). What it bounds is how many folders contend for the store's **single
/// write connection** at once, which past a handful is where a fan-out stops buying anything and
/// starts queueing. Deliberately the same order as the pool's per-account budget, so the two
/// numbers can be reconciled into one when it lands rather than fighting.
const MAX_CONCURRENT_FOLDERS: usize = 5;

/// Where a folder's sync spent its time.
///
/// **Reported rather than logged, because the engine is a library.** A log line is the host's
/// product surface — its wording, its level, its privacy rules — and a duration is a fact. The
/// same seam as `ConnectObserver`: the engine says what happened, the host decides how to say it.
///
/// The three do **not** sum to [`FolderSync::elapsed`]. What is left over is claiming and
/// releasing the scope lease, and the bookkeeping between chunks — small, and worth seeing as a
/// remainder rather than being folded into one of the buckets it is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncTiming {
    /// Awaiting the provider: the network, and whatever the adapter does to parse the wire.
    pub fetching: Duration,
    /// Projecting what arrived into rows — envelopes, the message-id graph, addresses.
    pub deriving: Duration,
    /// The store: the apply that commits a chunk, plus the recipient read it needs first.
    pub storing: Duration,
}

impl SyncTiming {
    /// Adds one phase's measurement to the running total.
    pub(crate) fn add_fetching(&mut self, at: Instant) {
        self.fetching += at.elapsed();
    }

    pub(crate) fn add_deriving(&mut self, at: Instant) {
        self.deriving += at.elapsed();
    }

    pub(crate) fn add_storing(&mut self, at: Instant) {
        self.storing += at.elapsed();
    }
}

/// One folder's outcome within an account pass.
#[derive(Debug)]
pub struct FolderSync {
    /// The folder's mail scope.
    pub scope: SyncScope,
    /// What it applied, or why it did not.
    pub result: Result<SyncApplied, SyncError>,
    /// Wall time for this folder alone. Folders overlap, so these do **not** sum to the pass.
    pub elapsed: Duration,
    /// Where that time went — network, projection, store.
    pub timing: SyncTiming,
}

/// What one account-level mail sync did.
///
/// **Returned whole, never behind a `Result`.** A partial failure is the ordinary case — one
/// folder busy, one refused, the rest fine — and collapsing that into a single `Err` throws away
/// exactly what a caller needs to tell an outage from an expired sign-in from a concurrent pass.
#[derive(Debug)]
pub struct MailSyncReport {
    /// The account-level **store** steps: the thread-index repair, the recipient backfill and the
    /// coverage record.
    ///
    /// An `Err` here is a store fault and never a network one, so a caller must not read it as
    /// the account being unreachable. That distinction is the whole reason it is its own field: a
    /// broken store that reports itself as an outage sends the user to check their wifi.
    pub account_steps: Result<(), SyncError>,
    /// The folder-list (container) sync, or `None` when this pass never looked at it — which is
    /// what [`refresh_folders`] does.
    ///
    /// `None` is not a success and not a failure: nothing was asked of the server, so a caller
    /// weighing whether the account is reachable must treat it the way it treats a scope another
    /// pass was holding, and read the folders instead.
    pub mailboxes: Option<Result<SyncApplied, SyncError>>,
    /// One entry per folder, in completion order.
    pub folders: Vec<FolderSync>,
    /// Wall time for the whole pass.
    pub elapsed: Duration,
}

impl MailSyncReport {
    /// Objects upserted across every folder that succeeded.
    #[must_use]
    pub fn upserted(&self) -> usize {
        self.folders
            .iter()
            .filter_map(|f| f.result.as_ref().ok())
            .map(|applied| applied.upserted)
            .sum()
    }

    /// Objects tombstoned across every folder that succeeded.
    #[must_use]
    pub fn tombstoned(&self) -> usize {
        self.folders
            .iter()
            .filter_map(|f| f.result.as_ref().ok())
            .map(|applied| applied.tombstoned)
            .sum()
    }

    /// Whether every scope this pass touched succeeded.
    ///
    /// A convenience for a caller that only needs "did this work" — anything deciding what to
    /// *tell the user* should read the fields, because "one folder was busy" and "the credential
    /// was refused" are different answers and this collapses them.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.account_steps.is_ok()
            && self.mailboxes.as_ref().is_none_or(Result::is_ok)
            && self.folders.iter().all(|f| f.result.is_ok())
    }

    /// The first failure this pass hit, in the order the steps ran.
    #[must_use]
    pub fn first_error(&self) -> Option<&SyncError> {
        self.account_steps
            .as_ref()
            .err()
            .or_else(|| self.mailboxes.as_ref().and_then(|r| r.as_ref().err()))
            .or_else(|| self.folders.iter().find_map(|f| f.result.as_ref().err()))
    }

    /// How many of this pass's scopes were skipped because another pass held them.
    #[must_use]
    pub fn busy_scopes(&self) -> usize {
        usize::from(
            self.mailboxes
                .as_ref()
                .is_some_and(|r| r.as_ref().is_err_and(SyncError::is_busy)),
        ) + self
            .folders
            .iter()
            .filter(|f| f.result.as_ref().is_err_and(SyncError::is_busy))
            .count()
    }

    /// How many folders this pass actually synced.
    #[must_use]
    pub fn folders_synced(&self) -> usize {
        self.folders.iter().filter(|f| f.result.is_ok()).count()
    }
}

/// Syncs one account's mail: the folder list once, then every folder.
///
/// `providers` is the account's mail providers — one per folder where the protocol binds a
/// connection to a mailbox (IMAP), a single element where one provider serves the account
/// (JMAP, Graph, Gmail). An empty slice is not an error; it reports a pass that did nothing.
///
/// The folder list is synced from the first provider, because it is a per-account container and
/// any of them can answer for it. Then the account-level store steps run **once** — where a host
/// fanning out per-folder syncs ran the recipient backfill and the coverage record once per
/// folder, concurrently, against the same store.
///
/// # Errors
///
/// None: every failure is reported in the [`MailSyncReport`], per scope. See its docs.
pub async fn sync_mail<P, S, O>(
    providers: &[P],
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    tuning: StreamTuning,
    observer: &O,
) -> MailSyncReport
where
    P: Provider,
    S: Store + StoreRead + ContactStore,
    O: SyncObserver,
{
    let started = Instant::now();
    let Some(first) = providers.first() else {
        observer.account_sync_started(account, 0, None);
        observer.account_sync_finished(account);
        return MailSyncReport {
            account_steps: Ok(()),
            mailboxes: None,
            folders: Vec::new(),
            elapsed: started.elapsed(),
        };
    };

    let req = LeaseRequest::new(worker.clone(), ttl);
    let repaired = repair_thread_index_if_damaged(store, account, worker, ttl).await;

    let mailboxes = run_scope(store, account, &MailboxScope(first), &req)
        .await
        .map(|run| run.into_applied().0);

    // Once per account, not once per folder. Both read and write the whole account's recipient
    // state, so running them per folder had every folder redo the same work concurrently.
    let sent = match recipients::sent_mailboxes(store, account).await {
        Ok(sent) => sent,
        Err(err) => return fail_early(observer, account, repaired, mailboxes, err, started),
    };
    if !sent.is_empty()
        && let Err(err) = recipients::backfill(store, account, &sent).await
    {
        return fail_early(observer, account, repaired, mailboxes, err, started);
    }

    let folders = run_folders(providers, store, account, &req, tuning, observer, &sent).await;

    let coverage = recipients::record_coverage(store, account, tuning.window, !sent.is_empty());
    let account_steps = repaired.and(coverage.await);
    observer.account_sync_finished(account);

    MailSyncReport {
        account_steps,
        mailboxes: Some(mailboxes),
        folders,
        elapsed: started.elapsed(),
    }
}

/// Syncs exactly the folders given, and **discovers nothing**.
///
/// The targeted counterpart of [`sync_mail`], for when the caller already knows which folder
/// changed — an IMAP `IDLE` push, a webhook, a folder the user just opened. It does no
/// account-level work at all: no folder-list sync, no thread-index repair, no recipient backfill,
/// no coverage record. `mailboxes` in the returned report is therefore `None`.
///
/// **This exists because the folder list is most of the cost, and a push has already told you the
/// answer it would give.** Measured against the Stalwart harness on a steady-state single-folder
/// pass, discovery was **57%** of the work on a `LIST-STATUS` server and **86%** on one without —
/// and the wire cost is worse than the wall clock says, because a server that cannot answer
/// `LIST-STATUS` is asked for a `STATUS` **per folder**: one extra round trip becomes fourteen on
/// a thirteen-folder account, on the path whose whole job is making new mail appear at once.
///
/// The one account-level thing it does keep is reading which mailboxes are Sent, because a
/// message landing in Sent must still be recorded as a recipient observation and that is a store
/// read with no round trip in it.
///
/// **Where new account-level work goes: [`sync_mail`], and only there.** This is a different
/// operation, not a second way to run a pass — its contract is that it does none of that — so
/// anything that must happen once per account belongs beside the repair and the recipient steps.
///
/// # Errors
///
/// None: every failure is reported in the [`MailSyncReport`], per scope. See its docs.
pub async fn refresh_folders<P, S, O>(
    providers: &[P],
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    tuning: StreamTuning,
    observer: &O,
) -> MailSyncReport
where
    P: Provider,
    S: Store + StoreRead + ContactStore,
    O: SyncObserver,
{
    let started = Instant::now();
    let req = LeaseRequest::new(worker, ttl);
    let sent = match recipients::sent_mailboxes(store, account).await {
        Ok(sent) => sent,
        Err(err) => {
            observer.account_sync_started(account, 0, None);
            observer.account_sync_finished(account);
            return MailSyncReport {
                account_steps: Err(err),
                mailboxes: None,
                folders: Vec::new(),
                elapsed: started.elapsed(),
            };
        }
    };
    let folders = run_folders(providers, store, account, &req, tuning, observer, &sent).await;
    observer.account_sync_finished(account);

    MailSyncReport {
        account_steps: Ok(()),
        mailboxes: None,
        folders,
        elapsed: started.elapsed(),
    }
}

/// Orders the folders, tells the observer what is coming, and streams them concurrently.
///
/// Shared by both entrypoints so the fan-out — its bound, its ordering and the events a host sees
/// — cannot drift between a full pass and a targeted refresh.
///
/// Resolving the Inbox is a **store** read, so a targeted refresh can afford it too: the ordering
/// matters when several folders are refreshed at once, and a host filing streaming rows by folder
/// needs the answer either way.
async fn run_folders<P, S, O>(
    providers: &[P],
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    tuning: StreamTuning,
    observer: &O,
    sent: &BTreeSet<MailboxId>,
) -> Vec<FolderSync>
where
    P: Provider,
    S: Store + StoreRead + ContactStore,
    O: SyncObserver,
{
    let inbox = stored_inbox(store, account).await;
    let order = inbox_first(account, providers, inbox.as_ref());
    observer.account_sync_started(account, order.len(), inbox.as_ref());

    let pass = FolderPass {
        store,
        req,
        tuning,
        observer,
        sent,
    };
    stream::iter(order.into_iter().map(|index| {
        let provider = &providers[index];
        let pass = &pass;
        async move {
            let started = Instant::now();
            let mut timing = SyncTiming::default();
            // Filled through even on the paths that return early, so a folder that failed still
            // says where it got to.
            let result = stream_email(provider, account, pass, &mut timing).await;
            let scope = provider.email_scope(account);
            observer.folder_sync_finished(account, &scope, result.is_ok());
            FolderSync {
                scope,
                result,
                elapsed: started.elapsed(),
                timing,
            }
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_FOLDERS)
    .collect::<Vec<_>>()
    .await
}

/// A pass that stopped at an account-level store step, before any folder ran.
fn fail_early<O: SyncObserver>(
    observer: &O,
    account: &AccountId,
    repaired: Result<(), SyncError>,
    mailboxes: Result<SyncApplied, SyncError>,
    err: SyncError,
    started: Instant,
) -> MailSyncReport {
    observer.account_sync_started(account, 0, None);
    observer.account_sync_finished(account);
    MailSyncReport {
        account_steps: repaired.and(Err(err)),
        mailboxes: Some(mailboxes),
        folders: Vec::new(),
        elapsed: started.elapsed(),
    }
}

/// Indices into `providers`, with the Inbox's first.
///
/// The Inbox is the folder the user is looking at, so filling it first is the difference between
/// a list that populates while the rest downloads and one that waits on whichever folder the
/// provider happened to hand over first. Everything else keeps its given order.
///
/// A provider whose scope names no mailbox (JMAP, Gmail, Graph — one provider for the account)
/// cannot be ordered against the others and does not need to be: there is only one.
fn inbox_first<P: Provider>(
    account: &AccountId,
    providers: &[P],
    inbox: Option<&MailboxId>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..providers.len()).collect();
    let Some(inbox) = inbox else {
        return order;
    };
    // Stable, so everything that is not the Inbox keeps the order the caller gave.
    order.sort_by_key(|&index| !names_mailbox(&providers[index].email_scope(account), inbox));
    order
}

/// The account's Inbox, from the folder list the pass has just synced.
async fn stored_inbox<S: StoreRead>(store: &S, account: &AccountId) -> Option<MailboxId> {
    let scopes = store.account_scopes(account.clone()).await.ok()?;
    for scope in scopes
        .into_iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Mailbox))
    {
        for (_key, payload) in store.scope_objects(&scope).await.ok()? {
            if let Ok(mailbox) = serde_json::from_value::<Mailbox>(payload)
                && mailbox.role == Some(MailboxRole::Inbox)
            {
                return Some(mailbox.id);
            }
        }
    }
    None
}

/// Whether a mail scope is the one for `mailbox`.
fn names_mailbox(scope: &SyncScope, mailbox: &MailboxId) -> bool {
    match scope {
        SyncScope::ImapMailbox { mailbox: named, .. }
        | SyncScope::GraphFolder { folder: named, .. } => named == mailbox,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use engine_core::ids::MailboxId;

    use super::*;

    fn imap(account: &AccountId, mailbox: &str) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: MailboxId::try_from(mailbox).unwrap(),
        }
    }

    /// The ordering decision itself, which is what the engine actually promises.
    ///
    /// Deliberately not asserted through a real pass: the folders then run concurrently and the
    /// order they *reach the network* is the order their lease claims resolve in, which is the
    /// scheduler's business and not a guarantee the engine makes or should be tested for. What it
    /// promises is where the Inbox sits in the queue it hands to the fan-out.
    #[test]
    fn the_inbox_is_ordered_first_and_the_rest_keep_their_order() {
        let account = AccountId::try_from("acct").unwrap();
        let inbox = MailboxId::try_from("INBOX").unwrap();
        let scopes = [
            imap(&account, "Archive"),
            imap(&account, "INBOX"),
            imap(&account, "Projects"),
        ];

        let mut order: Vec<usize> = (0..scopes.len()).collect();
        order.sort_by_key(|&index| !names_mailbox(&scopes[index], &inbox));

        assert_eq!(order, vec![1, 0, 2], "Inbox first, the others as given");
    }

    #[test]
    fn a_scope_that_names_no_mailbox_is_never_the_inbox() {
        // JMAP, Gmail and Graph give one provider for the whole account, so there is nothing to
        // order and nothing that could match.
        let account = AccountId::try_from("acct").unwrap();
        let jmap = SyncScope::JmapType {
            account,
            data_type: engine_core::sync::JmapDataType::Email,
        };
        assert!(!names_mailbox(
            &jmap,
            &MailboxId::try_from("INBOX").unwrap()
        ));
    }
}
