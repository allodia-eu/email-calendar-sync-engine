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
    MailboxScope, StreamTuning, SyncError, SyncObserver,
    mail_report::{FolderSync, MailSyncReport, SyncTiming},
    recipients, run_scope,
    stream::{FolderPass, stream_email},
    threading::repair_thread_index_if_damaged,
    vanished,
};

/// How many of an account's folders sync at once.
///
/// **A store figure, and deliberately still not a network one.** What it bounds is how many
/// folders contend for the store's **single write connection** at once, which past a handful is
/// where a fan-out stops buying anything and starts queueing.
///
/// It is tempting to make this the network bound too — `min` it with the
/// [`concurrent_fetches`](engine_provider::ConnectionInfo::concurrent_fetches) an adapter
/// publishes, so an account whose server allows four does not get five. Measuring said no, twice
/// over:
///
/// - **The ceiling is not per folder.** A live Microsoft 365 mailbox refuses at five *requests*,
///   whichever folders they belong to — `ApplicationThrottled: Application is over its
///   MailboxConcurrency limit`. An IMAP account is the opposite: each folder holds its own socket,
///   so its `concurrent_fetches` of `1` describes one connection and `min`ing it here would
///   serialize an account that is perfectly able to sync five folders at once.
/// - **Folder sync is not the only traffic.** The same mailbox, given four folder syncs plus one
///   body warm, refuses at that same five — and refuses both kinds. A bound that sees only the
///   fan-out cannot hold a total the server counts across everything, so clamping here would have
///   looked like a fix and left the warm to put the account straight back over.
///
/// The network bound is therefore `engine_http::RequestGate`, held once per account and shared
/// by every provider it connected, where every request passes and can be counted. This stays what
/// it says it is. A folder task that cannot get a lane simply waits at the gate, which costs a
/// parked future and nothing else.
const MAX_CONCURRENT_FOLDERS: usize = 5;

/// Syncs one account's mail: the folder list once, then every folder.
///
/// `providers` is the account's mail providers — one per folder wherever the protocol's mail
/// delta is per-folder (IMAP, and **Graph**, whose `email_scope` names a `GraphFolder`), a
/// single element where one provider serves the account (JMAP, Gmail). An empty slice is not an
/// error; it reports a pass that did nothing.
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
    // Before the fan-out, so a folder that has just left the list is not synced once more.
    let forgotten = match &mailboxes {
        Ok(_) => vanished::forget_vanished_folders(store, account, &req)
            .await
            .map(|_| ()),
        Err(_) => Ok(()),
    };

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
    let account_steps = repaired.and(forgotten).and(coverage.await);
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
