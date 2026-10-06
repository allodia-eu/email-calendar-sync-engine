//! Outbox-mediated writes: mail submission and edits ([`mail`]), and calendar writes
//! ([`calendar`]).
//!
//! Every write is **durable before any provider side effect** (`north-star.md` Write
//! Contract): the driver records a [`PendingOp`] carrying the request, claims it under a
//! fenced [`OpLease`](engine_store::OpLease) (the shared [`enqueue_and_claim`]), performs
//! the provider call, and records the outcome under that lease. Mail stamps the generated
//! `Message-ID` so the sent copy reconciles when it later syncs back; a calendar write
//! records the event key so the next sync reconciles the new revision.
//!
//! **The payload is the intent, not the rendered bytes.** A calendar patch stores the
//! `EventEdit` — which occurrence, and what changed — rather than the document it produced,
//! so a retry after a conflict can re-apply it to a *freshly fetched* base instead of
//! re-sending an edit built on a copy the server has moved past. Which adapter renders it,
//! and how, is not the outbox's business.
//!
//! These are the thin per-op drivers — one op, claimed and resolved inline. The background
//! worker that drains the outbox and honors `depends_on` chains is the later orchestrator.

mod calendar;
mod contact;
mod drafts;
mod drain;
mod drain_report;
mod mail;
mod mailbox;
mod mailbox_plan;
mod submit;

use core::time::Duration;

pub use calendar::{
    CalendarWriteOutcome, create_calendar_event, delete_calendar_event, patch_calendar_event,
    put_calendar_document, rsvp_calendar_event,
};
pub use contact::{ContactWriteOutcome, create_contact, delete_contact, patch_contact};
pub use drafts::{DraftPut, PutDraftOutcome, delete_draft_mail, put_draft_mail};
pub use drain::{DrainOutcome, DrainReport, DrainedOp, drain_outbox};
use engine_core::{
    ids::AccountId,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    ClaimRejection, LeaseRequest, LeasedPendingOp, OpLease, PendingOpClaim, Store, StoreError,
    WorkerId,
};
pub use mail::{
    MailEditOutcome, MailEditSent, ReportOutcome, edit_mail, mail_edit_op, queue_mail_edit,
    report_message, send_mail_edit,
};
pub use mailbox::{MailboxEditOutcome, edit_mailbox};
pub use mailbox_plan::{MailboxChange, MailboxNameError, MailboxPlace, validate_mailbox_name};
pub use submit::{SubmitOutcome, submit_mail};
// Tokio's own `Instant`, so the wait's bound holds under a paused test clock too.
use tokio::time::Instant;

use crate::SyncError;

/// How long a driver waits for another op to release the resource it needs.
///
/// Writes to one resource serialize, so a driver whose resource is in flight must wait
/// rather than give up: a host marks a message read on open and archives it a moment
/// later, and the archive arrives inside the mark-read's round trip. The bound is an
/// upper limit on one provider round trip, not an expected wait — the common case
/// clears in one poll. It is measured as **elapsed** time rather than as a count of
/// polls, because each poll also costs a store round trip: on a device where a sync is
/// committing, that round trip waits on the writer and dwarfs [`RESOURCE_POLL`], so
/// counting polls would hold the caller for a multiple of this bound. Past it the op
/// stays durably enqueued and the caller is told which condition refused it.
const RESOURCE_WAIT: Duration = Duration::from_secs(10);

/// How often the wait re-asks the store: one targeted claim per poll.
const RESOURCE_POLL: Duration = Duration::from_millis(25);

/// Durably records `op` (idempotent by its key) and claims it under a fenced lease,
/// returning the leased op ready to resolve. The shared head of every outbox driver
/// (`store-and-sync.md`): enqueue → claim, with the same fencing discipline as sync.
///
/// This is the **thin inline** primitive (the precedent `submit_mail` established): it
/// enqueues an op and claims it *right now* to resolve it in the same call. It claims
/// that op **by id**, so it leases nothing it will not resolve and nothing older can
/// starve it; a resource another op holds in flight is waited out up to
/// [`RESOURCE_WAIT`]. It is still not the background outbox worker: it runs only at the
/// moment of the enqueue, so an op it could not claim stays enqueued and unresolved,
/// and nothing retries it until a drainer exists.
async fn enqueue_and_claim<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    op: PendingOp,
) -> Result<LeasedPendingOp, SyncError> {
    let op_id = store.enqueue_pending_op(account.clone(), op).await?;
    claim_waiting(store, account, worker, ttl, op_id)
        .await?
        .map_err(|reason| {
            SyncError::Outbox(format!(
                "enqueued op {op_id:?} was not claimable: {reason:?}"
            ))
        })
}

/// Claims op `op_id` by id, waiting out a resource another op holds for up to
/// [`RESOURCE_WAIT`]; any other refusal, or a resource still held past it, is returned for the
/// caller to act on.
async fn claim_waiting<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    op_id: PendingOpId,
) -> Result<Result<LeasedPendingOp, ClaimRejection>, SyncError> {
    let deadline = Instant::now() + RESOURCE_WAIT;
    loop {
        let req = LeaseRequest::new(worker.clone(), ttl);
        match store.claim_pending_op(account.clone(), op_id, req).await? {
            PendingOpClaim::Leased(leased) => return Ok(Ok(*leased)),
            PendingOpClaim::Refused(ClaimRejection::Busy) if Instant::now() < deadline => {
                tokio::time::sleep(RESOURCE_POLL).await;
            }
            PendingOpClaim::Refused(reason) => return Ok(Err(reason)),
        }
    }
}

/// Runs `work` while renewing `lease` every third of `ttl`, and stops renewing when it
/// returns.
///
/// An attempt can outlast any fixed lease: a large upload on a slow line, a server that takes
/// minutes to accept a message. Without renewal its lease lapses mid-attempt and another
/// worker (a second drain, another process on the same store) recovers the op under it. A
/// renewal refused as stale means that happened anyway (the process was suspended past its
/// lease, say), and the attempt carries on: the hand-over record decides what it may still do,
/// and the store refuses what it may not. A renewal that fails for any other reason is tried
/// again at the next tick, which is still well inside the lease.
pub(crate) async fn holding<S: Store, F: Future>(
    store: &S,
    lease: &OpLease,
    ttl: Duration,
    work: F,
) -> F::Output {
    let renew = async {
        let every = ttl / 3;
        loop {
            tokio::time::sleep(every).await;
            if let Err(StoreError::StaleLease) = store.renew_pending_op_lease(lease, ttl).await {
                return core::future::pending::<core::convert::Infallible>().await;
            }
        }
    };
    tokio::select! {
        biased;
        done = work => done,
        never = renew => match never {},
    }
}

/// Records a failed write outcome with its classification and backoff hint.
///
/// Every calendar write is safe to retry — `PUT`/`DELETE` are idempotent HTTP methods (RFC
/// 7231 §4.2.2), a JMAP `/set` addresses the object by id, and the revision guard makes a
/// retry self-correcting — so, unlike an SMTP send whose post-`DATA` ack can be lost
/// ambiguously, a failed calendar write has no `NeedsConfirmation` case: every failure is a
/// plain classified `Failed`. The mail *edit* path shares this for the same reason
/// (`imap-smtp.md`); only [`submit_mail`] branches.
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
