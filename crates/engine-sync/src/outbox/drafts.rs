//! Keeping a draft on the server through the outbox: storing one ([`put_draft_mail`])
//! and removing one ([`delete_draft_mail`]).
//!
//! The shared outbox discipline applies (see the module docs): durable op → claim →
//! provider call → record the outcome under the lease. Two things are particular to a
//! draft, and both follow from a draft being saved *repeatedly* rather than once.
//!
//! **A queued save supersedes the one before it.** Saving five times with no network
//! should leave the server one write to do, not five rounds of store-and-remove against a
//! folder the user is watching. So enqueuing withdraws any unsettled save already queued
//! for the same draft. One a worker is holding is left alone: its provider call may be
//! happening right now, and the resource key serialises the new op behind it anyway.
//!
//! **The op is named by what it would store.** The idempotency key carries a hash of the
//! payload, so saving the same text twice is one op (a genuine retry dedups) while an
//! edited draft is a new one. A key naming only the draft would collide with its own
//! previous save; a key naming the attempt would never dedup at all.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{Draft, Provider};
use engine_store::{OpRejection, PendingOpState, Store, StoreRead, WorkerId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{enqueue_and_claim, record_failure};
use crate::SyncError;

/// What a stored-draft op carries, and what the drainer reads back out of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftPut {
    /// The draft to store.
    pub draft: Draft,
    /// The stored copy this one supersedes, if the draft has been saved before.
    #[serde(default)]
    pub replacing: Option<ProviderKey>,
}

/// The result of a successful draft save through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutDraftOutcome {
    /// The durable op that recorded the save.
    pub op: PendingOpId,
    /// The key the draft is now stored under.
    ///
    /// **Keep this, and pass it as `replacing` next time.** It is not necessarily the key
    /// that went in: three of the four adapters write a new message and remove the old, so
    /// the key moves (`providers.md`). A caller that assumes otherwise stores a second
    /// copy beside the first on the next save.
    pub key: ProviderKey,
}

/// Stores `draft` on the server through the outbox, superseding `replacing`.
///
/// # Errors
///
/// [`SyncError::Provider`] if the save failed (after recording it), [`SyncError::Store`]
/// on a store failure, or [`SyncError::Outbox`] if the payload could not be encoded.
pub async fn put_draft_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
) -> Result<PutDraftOutcome, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let request = DraftPut {
        draft: draft.clone(),
        replacing: replacing.cloned(),
    };
    let payload = serde_json::to_value(&request)
        .map_err(|e| SyncError::Outbox(format!("encode draft put: {e}")))?;
    let resource = resource_key(draft)?;
    let idempotency = IdempotencyKey::new(format!(
        "put-draft:{}:{}",
        draft.message_id.as_str(),
        digest(&payload)
    ))
    .map_err(|e| SyncError::Outbox(e.to_string()))?;

    withdraw_superseded(store, account, &resource, &idempotency).await?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, PendingOpKind::MailDraftPut, resource, payload),
    )
    .await?;

    match provider
        .put_draft(account, &request.draft, request.replacing.as_ref())
        .await
    {
        Ok(key) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: key.clone(),
                    },
                )
                .await?;
            Ok(PutDraftOutcome { op: leased.id, key })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Removes the stored draft `key` through the outbox.
///
/// # Errors
///
/// [`SyncError::Provider`] if the removal failed (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the payload could
/// not be encoded.
pub async fn delete_draft_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    key: &ProviderKey,
) -> Result<PendingOpId, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(key)
        .map_err(|e| SyncError::Outbox(format!("encode draft delete: {e}")))?;
    let idempotency = IdempotencyKey::new(format!("delete-draft:{}", key.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", key.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(
            idempotency,
            PendingOpKind::MailDraftDelete,
            resource,
            payload,
        ),
    )
    .await?;

    match provider.delete_draft(account, key).await {
        Ok(()) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: key.clone(),
                    },
                )
                .await?;
            Ok(leased.id)
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// The resource a draft's writes serialise on.
///
/// The same key a submission of that composition uses, so saving and sending the one
/// draft cannot run at once: whichever is claimed first holds the resource, and the other
/// waits rather than racing it.
fn resource_key(draft: &Draft) -> Result<ResourceKey, SyncError> {
    ResourceKey::new(format!("draft:{}", draft.message_id.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))
}

/// Withdraws a queued save this one replaces.
///
/// Only an unsettled save of the same draft, never the one this save would itself reuse,
/// and only one no worker holds: a leased op's provider call may already be in flight, so
/// withdrawing it would claim to have stopped something that has gone. That one is left to
/// finish, and the resource key keeps this save behind it.
async fn withdraw_superseded<S: Store + StoreRead>(
    store: &S,
    account: &AccountId,
    resource: &ResourceKey,
    enqueuing: &IdempotencyKey,
) -> Result<(), SyncError> {
    let queued = store.list_pending_ops(account.clone()).await?;
    for row in queued.into_iter().filter(|row| {
        row.kind == Some(PendingOpKind::MailDraftPut)
            && &row.resource_key == resource
            && row.state == PendingOpState::Pending
            // Never the op this save is about to reuse. Saving the same text twice is one
            // op by design, and withdrawing it here would cancel the row the enqueue then
            // dedups onto, leaving the save with nothing to claim.
            && &row.idempotency_key != enqueuing
    }) {
        // A refusal is not a failure: the op settled or was claimed between the read and
        // here, and either way this save still stores what the user has now.
        let _: Option<OpRejection> = store.cancel_pending_op(account.clone(), row.id).await?;
    }
    Ok(())
}

/// A short, stable digest of the payload, so the same save twice is the same op.
fn digest(payload: &serde_json::Value) -> String {
    let hash = Sha256::digest(payload.to_string().as_bytes());
    hash.iter().take(8).fold(String::new(), |mut hex, byte| {
        use core::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}
