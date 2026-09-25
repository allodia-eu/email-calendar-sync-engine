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
    write::{PendingOpId, PendingOpKind, PendingOutcome},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, SentCopy};
use engine_store::{
    LeaseRequest, LeasedPendingOp, PendingOpClaim, PendingOpRow, PendingOpState, Store, StoreRead,
    WorkerId,
};

use super::{drafts::DraftPut, mailbox_plan::MailboxChange, record_failure};
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
    /// The key the provider resolved the write to, when it succeeded and there is one.
    ///
    /// **This is the only place a caller learns what a queued write became.** An op that
    /// parked while offline succeeds with nobody watching, and the account then holds an
    /// object the caller has never seen a key for. A draft is what makes it matter: the
    /// next save has to name the copy it supersedes, or it stores a second one beside it,
    /// and on three of the four adapters the key is not the one that went in
    /// (`providers.md`).
    pub provider_key: Option<ProviderKey>,
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
    /// The stored payload could not be read as the request its kind names, so the op
    /// never reached the provider and is settled.
    ///
    /// Distinct from [`Failed`](DrainOutcome::Failed), which is the provider refusing:
    /// nothing was asked of it. A payload written by a build this one cannot read reaches
    /// here, and no number of retries changes that, so the op settles rather than
    /// blocking the pass behind it for ever.
    Undecodable {
        /// What could not be decoded.
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
        let ran = run_one(provider, store, account, &leased, op).await?;
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
/// A type rather than a subset of [`PendingOpKind`] checked by hand, so [`run_one`]
/// matches exhaustively. The alternative leaves a fallback arm for kinds
/// [`dispatchable`] already excluded: unreachable, untestable, and one edit away from
/// being neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailOp {
    Submit,
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
            Self::Edit => PendingOpKind::MailEdit,
            Self::Report => PendingOpKind::MailReport,
            Self::DraftPut => PendingOpKind::MailDraftPut,
            Self::DraftDelete => PendingOpKind::MailDraftDelete,
            Self::Mailbox => PendingOpKind::MailboxEdit,
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
        PendingOpKind::MailEdit => Some(MailOp::Edit),
        PendingOpKind::MailReport => Some(MailOp::Report),
        PendingOpKind::MailDraftPut => Some(MailOp::DraftPut),
        PendingOpKind::MailDraftDelete => Some(MailOp::DraftDelete),
        PendingOpKind::MailboxEdit => Some(MailOp::Mailbox),
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

/// Runs one claimed op and records its outcome under the lease it was claimed with.
async fn run_one<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    op: MailOp,
) -> Result<Ran, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    match op {
        MailOp::Submit => {
            let Some(draft) = decode::<Draft>(store, leased).await? else {
                return Ok(Ran::bare(undecodable("draft")));
            };
            match provider.submit_email(account, &draft).await {
                Ok(receipt) => {
                    let sent_key = receipt.email_key.clone();
                    store
                        .mark_pending_op(
                            &leased.lease,
                            PendingOutcome::Succeeded {
                                provider_key: receipt.email_key,
                            },
                        )
                        .await?;
                    Ok(Ran::keyed(
                        match receipt.sent_copy {
                            SentCopy::Filed => DrainOutcome::Succeeded,
                            SentCopy::Unfiled { detail } => DrainOutcome::SentNotFiled { detail },
                        },
                        sent_key,
                    ))
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
                        return Ok(Ran::bare(DrainOutcome::AwaitingConfirmation { detail }));
                    }
                    settle(store, leased, &err).await.map(Ran::bare)
                }
            }
        }
        MailOp::DraftPut => {
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
        MailOp::DraftDelete => {
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
        MailOp::Mailbox => {
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
        MailOp::Edit => {
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
        MailOp::Report => {
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
        Some(row) => DrainOutcome::Parked {
            class,
            attempts: row.attempts,
        },
        // Gone from the queue means it settled: the class was not retryable, or the
        // attempts ran out.
        None => DrainOutcome::Failed { class },
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
