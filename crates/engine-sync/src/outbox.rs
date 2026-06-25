//! Outbox-mediated writes: mail submission ([`submit_mail`]) and calendar
//! writes ([`write_calendar_event`]/[`delete_calendar_event`]).
//!
//! Every write is **durable before any provider side effect** (`north-star.md`
//! Write Contract): the driver records a [`PendingOp`] carrying the request, claims
//! it under a fenced [`OpLease`](engine_store::OpLease) (the shared
//! [`enqueue_and_claim`]), performs the provider call, and records the outcome under
//! that lease. Mail stamps the generated `Message-ID` so the sent copy reconciles
//! when it later syncs back; a calendar write records the resource href so the next
//! `sync-collection` delta reconciles the new revision.
//!
//! These are the thin per-op drivers — one op, claimed and resolved inline. The
//! background worker that drains the outbox and honors `depends_on` chains is the
//! later orchestrator.

use core::time::Duration;

use engine_core::ids::{AccountId, MessageIdHeader, ProviderKey, Uid};
use engine_core::version::ETag;
use engine_core::write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey};
use engine_provider::{Draft, EventDeletion, EventWrite, MailEdit, Provider};
use engine_store::{LeaseRequest, LeasedPendingOp, Store, WorkerId};

use crate::SyncError;

/// How many runnable ops a claim asks for (each driver resolves only its own).
const CLAIM_LIMIT: usize = 16;

/// Durably records `op` (idempotent by its key) and claims it under a fenced lease,
/// returning the leased op ready to resolve. The shared head of every outbox driver
/// (`store-and-sync.md`): enqueue → claim, with the same fencing discipline as sync.
///
/// This is the **thin inline** primitive (the precedent `submit_mail` established):
/// it enqueues an op and claims it *right now* to resolve it in the same call. It is
/// not the background outbox worker, so it inherits two limitations the worker will
/// remove (the worker claims runnable ops in id order and resolves whatever it gets):
/// it claims a bounded [`CLAIM_LIMIT`] batch and then finds its own op, so it errors
/// if the account already has ≥`CLAIM_LIMIT` older runnable ops; and a just-enqueued
/// op whose `resource_key` is already held by a live in-flight op is correctly
/// *deferred* by the store (not returned), which surfaces here as an error rather
/// than a wait. Both mean the inline driver assumes low outbox contention; under
/// real contention the orchestrator's worker is the right driver.
async fn enqueue_and_claim<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    op: PendingOp,
) -> Result<LeasedPendingOp, SyncError> {
    let op_id = store.enqueue_pending_op(account.clone(), op).await?;
    let req = LeaseRequest::new(worker, ttl);
    store
        .claim_pending_ops(account.clone(), req, CLAIM_LIMIT)
        .await?
        .into_iter()
        .find(|op| op.id == op_id)
        .ok_or_else(|| SyncError::Outbox(format!("enqueued op {op_id:?} was not claimable")))
}

/// The result of a successful submission through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// The durable op that recorded the send.
    pub op: PendingOpId,
    /// The provider key of the sent message (for reconciliation/threading).
    pub email_key: ProviderKey,
    /// The `Message-ID` that was sent.
    pub message_id: MessageIdHeader,
}

/// Sends `draft` through the outbox: durable op → claim → provider submit → record.
///
/// On a provider failure the op is recorded `Failed` (with the failure class) and
/// the error is returned — never blindly retried here.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the send fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the draft
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn submit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    draft: &Draft,
) -> Result<SubmitOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    // Durable record first: the draft as a pending op, idempotent by Message-ID.
    let payload =
        serde_json::to_value(draft).map_err(|e| SyncError::Outbox(format!("encode draft: {e}")))?;
    let message_id = draft.message_id.as_str();
    let idempotency = IdempotencyKey::new(format!("submit:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("draft:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, resource, payload),
    )
    .await?;

    // Provider side effect, then record the outcome under the lease.
    match provider.submit_email(account, draft).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.email_key.clone(),
                    },
                )
                .await?;
            Ok(SubmitOutcome {
                op: leased.id,
                email_key: receipt.email_key,
                message_id: receipt.message_id,
            })
        }
        Err(err) => {
            // An ambiguous send (e.g. a lost post-DATA SMTP ack) is parked for
            // confirmation, never recorded as a plain retryable failure — so the
            // outbox does not blind-retry and risk a double-send (`providers.md`).
            let outcome = if err.requires_confirmation() {
                PendingOutcome::NeedsConfirmation {
                    detail: err.detail().to_owned(),
                }
            } else {
                PendingOutcome::Failed {
                    class: err.class(),
                    retry_after: err.retry_after(),
                }
            };
            store.mark_pending_op(&leased.lease, outcome).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// The result of a successful calendar `PUT` through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarWriteOutcome {
    /// The durable op that recorded the write.
    pub op: PendingOpId,
    /// The provider key (resource href) now backing the object.
    pub event_key: ProviderKey,
    /// The new entity tag, if the server returned one on the `PUT` (else the next
    /// `sync-collection` delta carries it).
    pub etag: Option<ETag>,
    /// The event's `UID`, echoed for sync-time reconciliation.
    pub uid: Uid,
}

/// Creates or replaces a calendar event through the outbox: durable op → claim →
/// provider `PUT` → record.
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent — it
/// must be **unique per write intent** (the store dedups by `(account, key)` across
/// every op state, so a key derived only from the href would wrongly collapse two
/// distinct edits of the same resource into one op). The op's `resource_key` is the
/// href, so the store serializes writes to one event (a second write whose target is
/// already in flight is *deferred*; the thin inline driver assumes low outbox
/// contention — the background worker is the right driver under contention). A
/// provider failure is recorded `Failed` (with its
/// class) and returned — never blindly retried here. `PUT` and `DELETE` are
/// idempotent HTTP methods (RFC 7231 §4.2.2) — a re-`PUT` of the same body yields the
/// same resource, an already-gone `DELETE` resolves as success — and the
/// `If-Match`/`If-None-Match` precondition makes a retry self-correcting (a write
/// that already landed re-`412`s), so a later retry is safe (`caldav.md`).
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the `PUT` fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn write_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    write: &EventWrite,
) -> Result<CalendarWriteOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        write.href.as_str(),
        serde_json::to_value(write)
            .map_err(|e| SyncError::Outbox(format!("encode event write: {e}")))?,
    )
    .await?;

    match provider.put_event(account, write).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.event_key.clone(),
                    },
                )
                .await?;
            Ok(CalendarWriteOutcome {
                op: leased.id,
                event_key: receipt.event_key,
                etag: receipt.etag,
                uid: receipt.uid,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Deletes a calendar event through the outbox: durable op → claim → provider
/// `DELETE` → record. Returns the durable op id; the next sync tombstones the local
/// row.
///
/// `idempotency` must be unique per delete intent (see
/// [`write_calendar_event`]). A failure is recorded `Failed` and returned.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the `DELETE` fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn delete_calendar_event<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    deletion: &EventDeletion,
) -> Result<PendingOpId, SyncError>
where
    P: Provider,
    S: Store,
{
    let leased = enqueue_calendar_op(
        store,
        account,
        worker,
        ttl,
        idempotency,
        deletion.href.as_str(),
        serde_json::to_value(deletion)
            .map_err(|e| SyncError::Outbox(format!("encode event deletion: {e}")))?,
    )
    .await?;

    match provider.delete_event(account, deletion).await {
        Ok(()) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: deletion.href.key().clone(),
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

/// The result of a successful mail edit through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEditOutcome {
    /// The durable op that recorded the edit.
    pub op: PendingOpId,
    /// The provider key the edit resolved to (the edited message; for a move, its
    /// source key — the next sync reconciles the destination copy).
    pub message_key: ProviderKey,
}

/// Applies a [`MailEdit`] through the outbox: durable op → claim → provider
/// `edit_mail` → record. The mail counterpart of [`write_calendar_event`].
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent — it
/// must be **unique per edit intent** (the store dedups by `(account, key)` across
/// every op state, so a key derived only from the target would wrongly collapse two
/// distinct edits of one message — e.g. mark-read then mark-unread — into one op).
/// The op's `resource_key` is the target message key, so the store serializes edits
/// to one message (a second edit whose target is already in flight is *deferred*; the
/// thin inline driver assumes low outbox contention — the background worker is the
/// right driver under contention). A provider failure is recorded `Failed` (with its
/// class) and returned — never blindly retried here. Unlike an SMTP send there is no
/// `NeedsConfirmation` case: `UID STORE`/`MOVE`/`EXPUNGE` are not post-`DATA`-ambiguous
/// (a periodic snapshot reconciles the true state), and a stale-target `Conflict` is
/// self-correcting after a re-sync (`imap-smtp.md`).
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the edit fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn edit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    edit: &MailEdit,
) -> Result<MailEditOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(edit)
        .map_err(|e| SyncError::Outbox(format!("encode mail edit: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", edit.target().as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, resource, payload),
    )
    .await?;

    match provider.edit_mail(account, edit).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(MailEditOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Builds and claims a calendar write op: the payload under a caller-minted
/// idempotency key, serialized on the resource href so writes to one event never
/// race.
async fn enqueue_calendar_op<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    href: &str,
    payload: serde_json::Value,
) -> Result<LeasedPendingOp, SyncError> {
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource =
        ResourceKey::new(format!("caldav:{href}")).map_err(|e| SyncError::Outbox(e.to_string()))?;
    enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, resource, payload),
    )
    .await
}

/// Records a failed calendar write outcome. `PUT`/`DELETE` are idempotent HTTP
/// methods (RFC 7231 §4.2.2) and the ETag precondition makes a retry self-correcting,
/// so — unlike an SMTP send, whose post-`DATA` ack can be lost ambiguously — a failed
/// CalDAV write has no `NeedsConfirmation` case: every failure is a plain classified
/// `Failed`, safe to retry.
async fn record_failure<S: Store>(
    store: &S,
    leased: &LeasedPendingOp,
    err: &engine_provider::ProviderError,
) -> Result<(), SyncError> {
    store
        .mark_pending_op(
            &leased.lease,
            PendingOutcome::Failed {
                class: err.class(),
                retry_after: err.retry_after(),
            },
        )
        .await?;
    Ok(())
}
