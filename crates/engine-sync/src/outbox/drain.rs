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
//! **Mail only, so far.** [`Draft`], [`MailEdit`], [`MessageReport`] and a folder change
//! are complete in the payload: the provider call takes the account and the request and
//! nothing else (a folder change reads the folder list first, from the same provider). A
//! calendar patch or delete takes the `base` event *beside* the request, so draining one
//! means re-reading it from the store and re-applying the stored intent to it, which is
//! also the conflict recovery and is its own piece of work. Until then those ops stay
//! queued, untouched and counted as deferred, rather than being leased and abandoned.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, ProviderKey},
    write::{PendingOpKind, PendingOutcome},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, SentCopy};
use engine_store::{
    LeaseRequest, LeasedPendingOp, PendingOpClaim, PendingOpRow, PendingOpState, Store, StoreError,
    StoreRead, WorkerId,
};

pub use super::drain_report::{DrainOutcome, DrainReport, DrainedOp};
use super::{drafts::DraftPut, mailbox_plan::MailboxChange, record_failure, submit::Attempt};
use crate::SyncError;

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
        let Some(op) = dispatchable(&row) else {
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
        let ran = super::holding(
            store,
            &leased.lease,
            ttl,
            run_one(provider, store, account, &leased, op, ttl),
        )
        .await?;
        let Some(ran) = ran else {
            // Another worker took the op over while this pass was cut off from it.
            report.deferred += 1;
            continue;
        };
        report.attempted.push(DrainedOp {
            id: row.id,
            kind: op.kind(),
            outcome: ran.outcome,
            provider_key: ran.key,
        });
    }
    Ok(report)
}

/// The writes this pass runs: exactly the kinds whose provider call is complete in the
/// stored payload.
///
/// A type rather than a subset of [`PendingOpKind`] checked by hand, so [`run_one`] and
/// [`run_write`] match exhaustively. The alternative leaves a fallback arm for kinds
/// [`dispatchable`] already excluded: unreachable, untestable, and one edit away from
/// being neither. A send is its own variant because it runs under a hand-over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailOp {
    Submit,
    Write(Write),
}

/// The writes that send nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    Edit,
    Report,
    DraftPut,
    DraftDelete,
    Mailbox,
}

impl MailOp {
    /// The stored kind this dispatches, for the report a caller reads.
    fn kind(self) -> PendingOpKind {
        match self {
            Self::Submit => PendingOpKind::MailSubmit,
            Self::Write(Write::Edit) => PendingOpKind::MailEdit,
            Self::Write(Write::Report) => PendingOpKind::MailReport,
            Self::Write(Write::DraftPut) => PendingOpKind::MailDraftPut,
            Self::Write(Write::DraftDelete) => PendingOpKind::MailDraftDelete,
            Self::Write(Write::Mailbox) => PendingOpKind::MailboxEdit,
        }
    }
}

/// The write this pass would run for `row`, or `None` to leave it alone.
///
/// Three reasons to leave one: it carries no kind (enqueued before the store recorded
/// one, so nothing says which request type its payload is), it is not `Pending` (in
/// flight under someone else's lease, or awaiting a confirmation no retry may resolve),
/// or it is a kind whose provider call needs more than the payload.
fn dispatchable(row: &PendingOpRow) -> Option<MailOp> {
    if row.state != PendingOpState::Pending {
        return None;
    }
    match row.kind? {
        PendingOpKind::MailSubmit => Some(MailOp::Submit),
        PendingOpKind::MailEdit => Some(MailOp::Write(Write::Edit)),
        PendingOpKind::MailReport => Some(MailOp::Write(Write::Report)),
        PendingOpKind::MailDraftPut => Some(MailOp::Write(Write::DraftPut)),
        PendingOpKind::MailDraftDelete => Some(MailOp::Write(Write::DraftDelete)),
        PendingOpKind::MailboxEdit => Some(MailOp::Write(Write::Mailbox)),
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

/// What running one op produced: its outcome, and the key it resolved to if any.
struct Ran {
    outcome: DrainOutcome,
    key: Option<ProviderKey>,
}

impl Ran {
    /// An outcome that names nothing: a failure, a park, or a write with no key to keep.
    const fn bare(outcome: DrainOutcome) -> Self {
        Self { outcome, key: None }
    }

    /// A success that resolved to `key`.
    const fn keyed(outcome: DrainOutcome, key: ProviderKey) -> Self {
        Self {
            outcome,
            key: Some(key),
        }
    }
}

/// Runs one claimed op and records its outcome under the lease it was claimed with. `None`
/// when the outcome could not be recorded because the op is no longer this pass's.
async fn run_one<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    op: MailOp,
    ttl: Duration,
) -> Result<Option<Ran>, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    match op {
        MailOp::Submit => send(provider, store, account, leased, ttl).await,
        // A write whose lease was recovered under it (a lapsed lease, or another process's
        // start-up recovery) is no longer this pass's: it must not stop the rest of the queue.
        MailOp::Write(write) => match run_write(provider, store, account, leased, write).await {
            Ok(ran) => Ok(Some(ran)),
            Err(SyncError::Store(StoreError::StaleLease)) => Ok(None),
            Err(err) => Err(err),
        },
    }
}

/// Runs one claimed send (`submit::attempt`).
async fn send<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    ttl: Duration,
) -> Result<Option<Ran>, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let Some(draft) = decode::<Draft>(store, leased).await? else {
        return Ok(Some(Ran::bare(undecodable("draft"))));
    };
    Ok(Some(
        match super::submit::attempt(provider, store, account, leased, &draft, ttl).await? {
            Attempt::Delivered(receipt) => {
                let outcome = match receipt.sent_copy {
                    SentCopy::Filed => DrainOutcome::Succeeded,
                    SentCopy::Unfiled { detail } => DrainOutcome::SentNotFiled { detail },
                };
                Ran::keyed(outcome, receipt.email_key)
            }
            Attempt::Unknown(err) => Ran::bare(DrainOutcome::AwaitingConfirmation {
                detail: err.detail().to_owned(),
            }),
            Attempt::Refused(err) => Ran::bare(settled(store, leased, err.class()).await?),
            Attempt::Superseded(_) => return Ok(None),
        },
    ))
}

/// Runs one claimed write that is not a send.
async fn run_write<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    write: Write,
) -> Result<Ran, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    match write {
        Write::DraftPut => {
            let Some(request) = decode::<DraftPut>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("draft put")));
            };
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
                    // The key travels back in the report, because it is not necessarily
                    // the one that went in: a caller that queued this save while offline
                    // has to learn what the draft is stored under before it saves again,
                    // or it stores a second copy beside the first.
                    Ok(Ran::keyed(DrainOutcome::Succeeded, key))
                }
                Err(err) => settle(store, leased, &err).await.map(Ran::bare),
            }
        }
        Write::DraftDelete => {
            let Some(key) = decode::<ProviderKey>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("draft delete")));
            };
            match provider.delete_draft(account, &key).await {
                Ok(()) => {
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: key.clone(),
                            },
                        )
                        .await?;
                    Ok(Ran::keyed(DrainOutcome::Succeeded, key))
                }
                Err(err) => settle(store, leased, &err).await.map(Ran::bare),
            }
        }
        Write::Mailbox => {
            let Some(change) = decode::<MailboxChange>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("folder change")));
            };
            match super::mailbox::run(provider, store, account, leased, &change).await? {
                Ok(mailbox) => Ok(Ran {
                    outcome: DrainOutcome::Succeeded,
                    key: mailbox.map(|id| id.key().clone()),
                }),
                Err(err) => Ok(Ran::bare(settled(store, leased, err.class()).await?)),
            }
        }
        Write::Edit => {
            let Some(edit) = decode::<MailEdit>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("mail edit")));
            };
            match provider.edit_mail(account, &edit).await {
                Ok(receipt) => {
                    let edited = receipt.message_key.clone();
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.message_key,
                            },
                        )
                        .await?;
                    Ok(Ran::keyed(DrainOutcome::Succeeded, edited))
                }
                Err(err) => settle(store, leased, &err).await.map(Ran::bare),
            }
        }
        Write::Report => {
            let Some(report) = decode::<MessageReport>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("message report")));
            };
            match provider.report_message(account, &report).await {
                Ok(receipt) => {
                    let reported = receipt.message_key.clone();
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.message_key,
                            },
                        )
                        .await?;
                    Ok(Ran::keyed(DrainOutcome::Succeeded, reported))
                }
                Err(err) => settle(store, leased, &err).await.map(Ran::bare),
            }
        }
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
    settled(store, leased, err.class()).await
}

/// Whether the store parked or settled a failure already recorded against `leased`.
async fn settled<S: Store + StoreRead>(
    store: &S,
    leased: &LeasedPendingOp,
    class: FailureClass,
) -> Result<DrainOutcome, SyncError> {
    let row = store
        .list_pending_ops(leased.lease.account().clone())
        .await?
        .into_iter()
        .find(|row| row.id == leased.id);
    Ok(match row {
        Some(row) if row.state == PendingOpState::Pending => DrainOutcome::Parked {
            class,
            attempts: row.attempts,
        },
        // Settled: the class was not retryable, or the attempts ran out. A send that settles
        // stays listed, as `Failed`; anything else leaves the queue.
        _ => DrainOutcome::Failed { class },
    })
}

/// Deserializes a claimed op's payload into the request its kind names, settling the op
/// as permanently failed and returning `None` when it cannot be read.
///
/// A pass must not abort here. One op whose payload this build cannot read would
/// otherwise stop every op behind it draining, for ever, and the unreadable one is not
/// coming back however many times it is tried.
async fn decode<T: serde::de::DeserializeOwned>(
    store: &impl Store,
    leased: &LeasedPendingOp,
) -> Result<Option<T>, SyncError> {
    let Ok(request) = serde_json::from_value(leased.op.payload.clone()) else {
        store
            .mark_pending_op(
                &leased.lease,
                PendingOutcome::Failed {
                    class: FailureClass::Permanent,
                    retry_after: None,
                },
            )
            .await?;
        return Ok(None);
    };
    Ok(Some(request))
}

/// The outcome for an op whose payload could not be read.
fn undecodable(what: &str) -> DrainOutcome {
    DrainOutcome::Undecodable {
        detail: format!("queued {what} could not be read by this build"),
    }
}
