//! Contact create/patch/delete drivers through the durable outbox.

use core::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactPatch},
    ids::{AccountId, ContactId},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use engine_provider::{ContactWriteReceipt, ContactsProvider};
use engine_store::{LeasedPendingOp, Store, WorkerId};

use super::{enqueue_and_claim, record_failure};
use crate::SyncError;

/// Successful contact write identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactWriteOutcome {
    /// Durable operation.
    pub op: PendingOpId,
    /// Provider card identity.
    pub contact: ContactId,
}

/// Creates one explicitly targeted contact.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
pub async fn create_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    draft: &ContactDraft,
) -> Result<ContactWriteOutcome, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let resource = format!("contact-create:{}", draft.address_book.as_str());
    let leased =
        enqueue_contact_op(store, account, worker, ttl, idempotency, &resource, draft).await?;
    resolve(store, leased, provider.create_contact(account, draft).await).await
}

/// Patches one backing source card.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
#[allow(
    clippy::too_many_arguments,
    reason = "outbox lease parameters plus the source base and patch intent"
)]
pub async fn patch_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &ContactCard,
    patch: &ContactPatch,
) -> Result<ContactWriteOutcome, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let leased = enqueue_contact_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &format!("contact:{}", base.id.as_str()),
        patch,
    )
    .await?;
    resolve(
        store,
        leased,
        provider.patch_contact(account, base, patch).await,
    )
    .await
}

/// Deletes one backing source card.
///
/// # Errors
///
/// Returns [`SyncError`] for outbox/store failures or a classified provider failure.
pub async fn delete_contact<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    base: &ContactCard,
) -> Result<PendingOpId, SyncError>
where
    P: ContactsProvider,
    S: Store,
{
    let leased = enqueue_contact_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        &format!("contact:{}", base.id.as_str()),
        &base.id,
    )
    .await?;
    match provider.delete_contact(account, base).await {
        Ok(()) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: base.id.key().clone(),
                    },
                )
                .await?;
            Ok(leased.id)
        }
        Err(error) => {
            record_failure(store, &leased, &error).await?;
            Err(SyncError::Provider(error))
        }
    }
}

async fn resolve<S: Store>(
    store: &S,
    leased: LeasedPendingOp,
    result: engine_provider::ProviderResult<ContactWriteReceipt>,
) -> Result<ContactWriteOutcome, SyncError> {
    match result {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.contact.key().clone(),
                    },
                )
                .await?;
            Ok(ContactWriteOutcome {
                op: leased.id,
                contact: receipt.contact,
            })
        }
        Err(error) => {
            record_failure(store, &leased, &error).await?;
            Err(SyncError::Provider(error))
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "outbox lease parameters and serialized operation identity"
)]
async fn enqueue_contact_op<S: Store, T: serde::Serialize>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    resource: &str,
    request: &T,
) -> Result<LeasedPendingOp, SyncError> {
    let payload = serde_json::to_value(request)
        .map_err(|error| SyncError::Outbox(format!("encode contact write: {error}")))?;
    let idempotency =
        IdempotencyKey::new(idempotency).map_err(|error| SyncError::Outbox(error.to_string()))?;
    let resource =
        ResourceKey::new(resource).map_err(|error| SyncError::Outbox(error.to_string()))?;
    enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, resource, payload),
    )
    .await
}
