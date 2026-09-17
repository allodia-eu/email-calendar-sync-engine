//! The background drainer: the pass that finally attempts what the inline drivers
//! recorded and left behind.
//!
//! The inline drivers run one op at the moment a user asks for it. Anything that failed
//! stayed durably enqueued and nothing came back for it, so a write attempted without a
//! network silently never happened (issue #60). This is what comes back.
//!
//! **It claims only what it can run.** A pass reads the account's queue, keeps the ops
//! whose [`PendingOpKind`] it dispatches, and takes each one under a *targeted* claim. The
//! batch claim would lease whatever is runnable, including kinds this pass cannot
//! dispatch, and an op leased by a worker that will not resolve it is held for its whole
//! lease: the failure #202 removed from the inline path, which must not come back here.
//!
//! **Mail only, so far.** [`Draft`], [`MailEdit`] and [`MessageReport`] are complete in
//! the payload: the provider call takes the account and the request and nothing else. A
//! calendar patch or delete takes the `base` event *beside* the request, so draining one
//! means re-reading it from the store and re-applying the stored intent to it, which is
//! also the conflict recovery and is its own piece of work. Until then those ops stay
//! queued, untouched and counted as deferred, rather than being leased and abandoned.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    write::{PendingOpId, PendingOpKind, PendingOutcome},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, SentCopy};
use engine_store::{
    LeaseRequest, LeasedPendingOp, PendingOpClaim, PendingOpRow, PendingOpState, Store, StoreRead,
    WorkerId,
};

use super::record_failure;
use crate::SyncError;

/// What one drain pass did, one entry per op it attempted.
///
/// A pass that attempted nothing is not an error: an empty outbox, ops still waiting out a
/// backoff, and a queue of kinds this pass cannot dispatch all reach it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DrainReport {
    /// The ops this pass attempted, in the order it took them.
    pub attempted: Vec<DrainedOp>,
    /// Ops left for a later pass: not yet due, serialized behind another op, or of a kind
    /// this pass does not dispatch. None of them was leased.
    pub deferred: usize,
}

impl DrainReport {
    /// How many provider calls succeeded, a delivered-but-unfiled send included: the
    /// message went out either way, which is the fact a caller acts on.
    #[must_use]
    pub fn delivered(&self) -> usize {
        self.attempted
            .iter()
            .filter(|op| {
                matches!(
                    op.outcome,
                    DrainOutcome::Succeeded | DrainOutcome::SentNotFiled { .. }
                )
            })
            .count()
    }

    /// Whether this pass changed nothing, so a caller can skip a refresh.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.attempted.is_empty()
    }
}

/// One op a drain pass attempted, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainedOp {
    /// The durable op.
    pub id: PendingOpId,
    /// Which write it was.
    pub kind: PendingOpKind,
    /// What the provider call did.
    pub outcome: DrainOutcome,
}

/// The outcome of one drained op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// The provider call succeeded and the op settled.
    Succeeded,
    /// A submission was **delivered** and the sender's copy was not filed.
    ///
    /// Its own variant because the two facts have to travel together: folding it into
    /// [`Succeeded`](DrainOutcome::Succeeded) loses the copy in silence, and folding it
    /// into a failure invites re-sending mail the recipients already have. The op is
    /// settled either way — the mail has gone.
    SentNotFiled {
        /// Why filing failed: a class and protocol detail, never draft content.
        detail: String,
    },
    /// The call failed retryably, so the op is queued again for a later pass.
    Parked {
        /// How it failed.
        class: FailureClass,
        /// How many attempts it has now had.
        attempts: u32,
    },
    /// The call failed in a way no retry fixes, or the op ran out of attempts. It will
    /// not be attempted again.
    Failed {
        /// How it failed.
        class: FailureClass,
    },
    /// A send whose outcome is genuinely ambiguous: parked for confirmation and **never**
    /// retried, so the outbox cannot double-send.
    AwaitingConfirmation {
        /// The provider's description of the ambiguity.
        detail: String,
    },
}

/// Attempts every op in `account`'s outbox that is due and that this pass can dispatch.
///
/// The host decides *when*: on reconnect (it owns the reachability signal), after a sync,
/// or when a user asks. The engine polls nothing — a timer here would wake a dead network
/// on a battery.
///
/// # Errors
///
/// Returns [`SyncError::Store`] if the queue cannot be read or an outcome cannot be
/// recorded. A **provider** failure is not an error: it is recorded against its op and
/// reported in the [`DrainReport`], because one unreachable recipient must not stop the
/// rest of the queue going out.
pub async fn drain_outbox<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<DrainReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let queue = store.list_pending_ops(account.clone()).await?;
    let mut report = DrainReport::default();

    for row in queue {
        let Some(kind) = dispatchable(&row) else {
            report.deferred += 1;
            continue;
        };
        let req = LeaseRequest::new(worker.clone(), ttl);
        // Targeted: this pass leases exactly the op it is about to run. A refusal is the
        // store's answer that the op is not this pass's to take (still backing off, or
        // serialized behind a live write), not a failure.
        let PendingOpClaim::Leased(leased) =
            store.claim_pending_op(account.clone(), row.id, req).await?
        else {
            report.deferred += 1;
            continue;
        };
        let outcome = run_one(provider, store, account, &leased, kind).await?;
        report.attempted.push(DrainedOp {
            id: row.id,
            kind,
            outcome,
        });
    }
    Ok(report)
}

/// The kind this pass would dispatch for `row`, or `None` to leave it alone.
///
/// Three reasons to leave one: it carries no kind (enqueued before the store recorded
/// one, so nothing says which request type its payload is), it is not `Pending` (in
/// flight under someone else's lease, or awaiting a confirmation no retry may resolve),
/// or it is a kind whose provider call needs more than the payload.
fn dispatchable(row: &PendingOpRow) -> Option<PendingOpKind> {
    if row.state != PendingOpState::Pending {
        return None;
    }
    match row.kind? {
        kind
        @ (PendingOpKind::MailSubmit | PendingOpKind::MailEdit | PendingOpKind::MailReport) => {
            Some(kind)
        }
        PendingOpKind::CalendarCreate
        | PendingOpKind::CalendarPatch
        | PendingOpKind::CalendarDocument
        | PendingOpKind::CalendarRsvp
        | PendingOpKind::CalendarDelete
        | PendingOpKind::ContactCreate
        | PendingOpKind::ContactPatch
        | PendingOpKind::ContactDelete => None,
    }
}

/// Runs one claimed op and records its outcome under the lease it was claimed with.
async fn run_one<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    kind: PendingOpKind,
) -> Result<DrainOutcome, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    match kind {
        PendingOpKind::MailSubmit => {
            let draft: Draft = decode(leased, "draft")?;
            match provider.submit_email(account, &draft).await {
                Ok(receipt) => {
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.email_key,
                            },
                        )
                        .await?;
                    Ok(match receipt.sent_copy {
                        SentCopy::Filed => DrainOutcome::Succeeded,
                        SentCopy::Unfiled { detail } => DrainOutcome::SentNotFiled { detail },
                    })
                }
                Err(err) => {
                    // An ambiguous send is parked, never recorded as a retryable failure:
                    // the outbox must not risk putting it in front of its recipients twice.
                    if err.requires_confirmation() {
                        let detail = err.detail().to_owned();
                        store
                            .mark_pending_op(
                                &leased.lease,
                                PendingOutcome::NeedsConfirmation {
                                    detail: detail.clone(),
                                },
                            )
                            .await?;
                        return Ok(DrainOutcome::AwaitingConfirmation { detail });
                    }
                    settle(store, leased, &err).await
                }
            }
        }
        PendingOpKind::MailEdit => {
            let edit: MailEdit = decode(leased, "mail edit")?;
            match provider.edit_mail(account, &edit).await {
                Ok(receipt) => {
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.message_key,
                            },
                        )
                        .await?;
                    Ok(DrainOutcome::Succeeded)
                }
                Err(err) => settle(store, leased, &err).await,
            }
        }
        PendingOpKind::MailReport => {
            let report: MessageReport = decode(leased, "message report")?;
            match provider.report_message(account, &report).await {
                Ok(receipt) => {
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.message_key,
                            },
                        )
                        .await?;
                    Ok(DrainOutcome::Succeeded)
                }
                Err(err) => settle(store, leased, &err).await,
            }
        }
        // `dispatchable` admits no other kind.
        other => Err(SyncError::Outbox(format!(
            "drain does not dispatch {other:?}"
        ))),
    }
}

/// Records a provider failure and reports whether the store parked or settled it.
///
/// The store owns that decision (`store-and-sync.md`): it counts the attempt and compares
/// the class against the attempt bound. Reading the state back after the write is what
/// keeps this from being a second, disagreeing copy of the rule.
async fn settle<S: Store + StoreRead>(
    store: &S,
    leased: &LeasedPendingOp,
    err: &engine_provider::ProviderError,
) -> Result<DrainOutcome, SyncError> {
    record_failure(store, leased, err).await?;
    let class = err.class();
    let row = store
        .list_pending_ops(leased.lease.account().clone())
        .await?
        .into_iter()
        .find(|row| row.id == leased.id);
    Ok(match row {
        Some(row) => DrainOutcome::Parked {
            class,
            attempts: row.attempts,
        },
        // Gone from the queue means it settled: the class was not retryable, or the
        // attempts ran out.
        None => DrainOutcome::Failed { class },
    })
}

/// Deserializes a claimed op's payload into the request its kind names.
fn decode<T: serde::de::DeserializeOwned>(
    leased: &LeasedPendingOp,
    what: &str,
) -> Result<T, SyncError> {
    serde_json::from_value(leased.op.payload.clone())
        .map_err(|e| SyncError::Outbox(format!("decode queued {what}: {e}")))
}
