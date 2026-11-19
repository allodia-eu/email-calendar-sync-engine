//! Folder-tree changes through the outbox: durable op, a check against the server's folder
//! list, the provider call, the outcome.
//!
//! The payload is the [`MailboxChange`] the host asked for, never the
//! [`MailboxEdit`](engine_provider::MailboxEdit) it came to: which name a trashed folder takes, and
//! whether the folder is still where the user saw it, are decided when the op **runs**, against the
//! list as the server has it then ([`mailbox_plan`](super::mailbox_plan)). A change that waited out
//! an afternoon offline is judged against the afternoon's end, not its start.
//!
//! Every change to one account's tree serialises on one resource. On IMAP a folder's key is
//! its path, so renaming a folder moves the key of every folder inside it; two changes to
//! one tree running at once could each name a folder the other has just moved.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Mailbox,
    sync::SyncUpdate,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{Provider, ProviderError};
use engine_store::{LeasedPendingOp, Store, WorkerId};

use super::{
    enqueue_and_claim,
    mailbox_plan::{MailboxChange, Plan, plan},
    record_failure,
};
use crate::SyncError;

/// The resource every change to one account's folder tree serialises on.
const TREE_RESOURCE: &str = "mailbox-tree";

/// The result of a folder change that went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxEditOutcome {
    /// The durable op that recorded the change.
    pub op: PendingOpId,
    /// The folder the change resolved to: the new folder, or the changed folder's key
    /// **after** the change, which on IMAP is not the key it had. `None` when no folder
    /// remains.
    pub mailbox: Option<MailboxId>,
}

/// Changes the account's folder tree through the outbox: durable op → claim → check the
/// server's folder list → provider `edit_mailbox` → record.
///
/// `idempotency` must be unique per change the user made, for the reason it must be on a
/// mail edit: two renames of one folder are two ops.
///
/// A failure is recorded against the op and returned. A retryable one (no network) parks
/// the op for [`drain_outbox`](super::drain_outbox), which runs the same check before it
/// sends, so a change queued offline is still refused if the folder changed elsewhere in
/// the meantime.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the change is refused or fails (after recording it),
/// with a name no transport accepts refused before anything is queued;
/// [`SyncError::Store`] on a store failure; or [`SyncError::Outbox`] if the change cannot
/// be encoded or the just-enqueued op is not claimable.
pub async fn edit_mailbox<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    change: &MailboxChange,
) -> Result<MailboxEditOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    change
        .validate()
        .map_err(|err| SyncError::Provider(ProviderError::invalid_state(err.to_string())))?;
    let payload = serde_json::to_value(change)
        .map_err(|e| SyncError::Outbox(format!("encode folder change: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(TREE_RESOURCE).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(
            idempotency_key,
            PendingOpKind::MailboxEdit,
            resource,
            payload,
        ),
    )
    .await?;

    match run(provider, store, account, &leased, change).await? {
        Ok(mailbox) => Ok(MailboxEditOutcome {
            op: leased.id,
            mailbox,
        }),
        Err(err) => Err(SyncError::Provider(err)),
    }
}

/// Runs one claimed folder change and records its outcome under the lease: the half the
/// inline driver and the drainer share.
///
/// The outer `Result` is the store; the inner one is the provider's answer, already
/// recorded against the op either way.
pub(super) async fn run<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    change: &MailboxChange,
) -> Result<Result<Option<MailboxId>, ProviderError>, SyncError>
where
    P: Provider,
    S: Store,
{
    match attempt(provider, account, change).await {
        Ok(mailbox) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: settled_key(change, mailbox.as_ref()),
                    },
                )
                .await?;
            Ok(Ok(mailbox))
        }
        Err(err) => {
            record_failure(store, leased, &err).await?;
            Ok(Err(err))
        }
    }
}

/// Reads the folder list, decides what the change still means, and sends it if anything is
/// left to send.
async fn attempt<P: Provider>(
    provider: &P,
    account: &AccountId,
    change: &MailboxChange,
) -> Result<Option<MailboxId>, ProviderError> {
    let current = current_folders(provider, account).await?;
    match plan(change, &current)? {
        Plan::Done(mailbox) => Ok(mailbox),
        Plan::Send(edit) => {
            let receipt = provider.edit_mailbox(account, &edit).await?;
            Ok(receipt.mailbox)
        }
    }
}

/// The account's folders as the server has them now: a full read, never the delta the
/// sync cursor would give.
async fn current_folders<P: Provider>(
    provider: &P,
    account: &AccountId,
) -> Result<Vec<Mailbox>, ProviderError> {
    Ok(match provider.sync_mailboxes(account, None).await?.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    })
}

/// The key the outbox records a settled change under: the folder it resolved to, else the
/// one it named.
fn settled_key(change: &MailboxChange, mailbox: Option<&MailboxId>) -> ProviderKey {
    let named = match change {
        MailboxChange::Create { parent, .. } => parent.as_ref(),
        MailboxChange::Update { target, .. }
        | MailboxChange::Trash { target, .. }
        | MailboxChange::Delete { target } => Some(target),
    };
    mailbox.or(named).map_or_else(
        || ProviderKey::new(TREE_RESOURCE).expect("a non-empty literal is a valid key"),
        |id| id.key().clone(),
    )
}
