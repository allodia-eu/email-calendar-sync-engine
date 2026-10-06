//! The outbox-mediated **mail** writes on `Engine` — submission, edits and reporting a
//! message as junk / not junk / phishing — plus the
//! pending-op state poll every write (mail and calendar alike) is observed through. The
//! calendar writes live in `calendar_writes`, which additionally reconciles the store.

use engine_core::{
    ids::{AccountId, ProviderKey},
    write::{PendingOpId, PendingOpKind},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider};
use engine_store::{Confirmation, OpRejection, PendingOpRow, PendingOpState, Store, StoreRead};
use engine_sync::{
    DrainReport, MailEditSent, PutDraftOutcome, ReportOutcome, SubmitOutcome, SyncError,
    delete_draft_mail, drain_outbox, edit_mail, put_draft_mail, queue_mail_edit, report_message,
    send_mail_edit, submit_mail,
};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Submits `draft` for one account through the durable outbox: the draft is
    /// recorded as a pending op (idempotent by its `Message-ID`) **before** the
    /// provider send, so a crash or an ambiguous failure never loses or double-sends
    /// it (`north-star.md` Write Contract). Returns the sent message's key, its
    /// `Message-ID`, and the op id — pollable via [`Engine::pending_op_state`].
    ///
    /// Whatever ends the process during the call, the send is not lost and not delivered
    /// twice: the attempt records its hand-over to the server before the first byte that
    /// could deliver it, so the next process ([`recover_interrupted_ops`], or any drain once
    /// the lease has lapsed) retries a send that never left and asks about one that may have.
    ///
    /// [`recover_interrupted_ops`]: Self::recover_interrupted_ops
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the send fails: the op is first recorded
    /// `Failed` (with the failure class), or `NeedsConfirmation` when the message may have
    /// been delivered — the outbox never blind-retries — and the error then returns. A failed
    /// send stays in [`outbox`](Self::outbox). A store failure also surfaces as
    /// [`ApiError::Sync`].
    pub async fn submit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
    ) -> Result<SubmitOutcome, ApiError> {
        submit_mail(provider, &self.store, account, worker(), LEASE_TTL, draft)
            .await
            .map_err(map_sync_error)
    }

    /// Stores `draft` on the server through the outbox, superseding `replacing`.
    ///
    /// **Keep the key that comes back and pass it as `replacing` next time.** It is not
    /// necessarily the one that went in: three of the four adapters write a new message
    /// and remove the old, so the key moves, while Gmail's draft object survives
    /// (`providers.md`). A caller that assumes otherwise stores a second copy beside the
    /// first on the next save.
    ///
    /// A save with no network is **kept**, not lost: the durable op parks and a later
    /// [`drain_outbox`](Self::drain_outbox) attempts it. Saving again before that happens
    /// replaces what was queued, so an edited draft costs the server one write rather
    /// than one per save.
    ///
    /// # Errors
    ///
    /// [`ApiError`] if the save failed; the op is recorded either way.
    pub async fn put_draft<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
        replacing: Option<&ProviderKey>,
    ) -> Result<PutDraftOutcome, ApiError> {
        put_draft_mail(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            draft,
            replacing,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Removes the stored draft `key` through the outbox: the user discarded it, or it
    /// has been sent and the copy in Drafts would otherwise linger.
    ///
    /// A draft that is already gone settles as done rather than failing, so a removal
    /// that is retried after a lost response does not park forever.
    ///
    /// # Errors
    ///
    /// [`ApiError`] if the removal failed; the op is recorded either way.
    pub async fn delete_draft<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<PendingOpId, ApiError> {
        delete_draft_mail(provider, &self.store, account, worker(), LEASE_TTL, key)
            .await
            .map_err(map_sync_error)
    }

    /// Files the sender's copy of an **already-delivered** message, repairing a submission
    /// whose [`SubmitOutcome::sent_copy`] came back
    /// [`SentCopy::Unfiled`](engine_provider::SentCopy::Unfiled). Returns the filed copy's
    /// key.
    ///
    /// **Not outbox-mediated, deliberately.** The outbox exists so a side effect is never
    /// lost or repeated across a crash; this one sends nothing and the provider makes it
    /// idempotent by probing for the copy first, so a durable op would buy nothing and would
    /// add a second op recording the same submission. It is a repair a user asks for, and
    /// asking again is free.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the copy still could not be filed — the message stays
    /// sent either way, and the caller may offer the retry again.
    pub async fn file_sent_copy<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
    ) -> Result<ProviderKey, ApiError> {
        provider
            .file_sent_copy(account, draft)
            .await
            .map_err(|err| map_sync_error(SyncError::Provider(err)))
    }

    /// Applies a [`MailEdit`] to one of the account's messages through the durable outbox, in
    /// one call: [`Engine::queue_mail_edit`], then [`Engine::send_mail_edit`].
    ///
    /// A mark-read/flag (`SetKeywords`), a move to another folder (`MoveTo`, also the mechanism
    /// behind a Trash "delete", the host resolving the Trash mailbox), or a permanent delete
    /// (`Delete`). A host that shows the edit before the server answers queues and sends in two
    /// calls instead, and redraws between them.
    ///
    /// # Errors
    ///
    /// As [`Engine::queue_mail_edit`] and [`Engine::send_mail_edit`].
    pub async fn edit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        edit: &MailEdit,
    ) -> Result<MailEditSent, ApiError> {
        edit_mail(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            edit,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Queues `edit` durably and returns its op, without reaching the server.
    ///
    /// The op is recorded **before** any provider side effect, so a crash never loses it
    /// (`north-star.md` Write Contract), and needs no connection: an edit made offline queues the
    /// same way and goes out with the next [`Engine::drain_outbox`]. `idempotency` must be
    /// **unique per edit intent**: deriving it only from the target message would collapse
    /// mark-read then mark-unread into one op.
    ///
    /// **A keyword change is shown from now on.** Every read of the message carries it until the
    /// server accepts it, when the next sync confirms it, or refuses it for good, when it is
    /// taken back. A sync in between does not undo it (`store-and-sync.md`). A move or a delete
    /// is not shown: the store learns of it from the next sync.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the edit cannot be encoded or the store fails.
    pub async fn queue_mail_edit(
        &self,
        account: &AccountId,
        idempotency: &str,
        edit: &MailEdit,
    ) -> Result<PendingOpId, ApiError> {
        queue_mail_edit(&self.store, account, idempotency, edit)
            .await
            .map_err(map_sync_error)
    }

    /// Sends the queued edit `op` now. `edit` is the edit it was queued with.
    ///
    /// [`MailEditSent::Applied`] carries the resolved message key; the next
    /// [`Engine::sync_mail`] reconciles the rest of the stored message. A refusal the outbox will
    /// retry (offline, rate limited, a timeout, another write to the message still in flight) is
    /// not an error but [`MailEditSent::Queued`]: the op waits for [`Engine::drain_outbox`], and a
    /// keyword change stays shown meanwhile.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] when the server refused the edit for good, after the store has
    /// taken back a keyword change it showed (a stale-target `Conflict`, such as an IMAP UID
    /// under a changed `UIDVALIDITY`, means re-sync then retry); when the op is unknown or already
    /// settled; or when the store fails.
    pub async fn send_mail_edit<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        op: PendingOpId,
        edit: &MailEdit,
    ) -> Result<MailEditSent, ApiError> {
        send_mail_edit(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            op,
            edit,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Reports one of the account's messages to its provider as junk, not junk, or
    /// phishing, through the durable outbox. The message is also **filed** — into
    /// `report.destination`, which the caller resolves (the account's Junk mailbox, or
    /// its Inbox for a not-junk verdict), exactly as it resolves Trash for a delete.
    ///
    /// **Read
    /// [`Capabilities::mail_report`](engine_provider::Capabilities::mail_report) first.**
    /// It is `None` where the provider cannot report at all, and its
    /// [`verdicts`](engine_provider::ReportControls::verdicts) are not universal — Gmail
    /// has no phishing verdict, and an adapter asked for one it lacks **refuses** rather
    /// than filing it as junk. Its
    /// [`evidence`](engine_provider::ReportControls::evidence) says whether the provider
    /// acknowledges the report or merely receives a convention, which is what a caller
    /// needs before telling a user what reporting will achieve.
    ///
    /// `idempotency` must be **unique per report intent** — a key derived only from the
    /// target would collapse a junk report and a later not-junk correction of the same
    /// message into one op.
    ///
    /// The next [`Engine::sync_mail`] reconciles the local rows to the new server state.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the report fails: the op is first recorded `Failed`
    /// (with the failure class), and the error then returns. A store failure also
    /// surfaces as [`ApiError::Sync`].
    pub async fn report_message<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        report: &MessageReport,
    ) -> Result<ReportOutcome, ApiError> {
        report_message(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            report,
        )
        .await
        .map_err(map_sync_error)
    }

    /// The current lifecycle state of a pending outbox op — e.g. the one a
    /// [`submit_mail`](Self::submit_mail) returned — or `None` if no such op exists.
    /// A lease-free read, safe to poll for write progress and confirmation state.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn pending_op_state(
        &self,
        op: PendingOpId,
    ) -> Result<Option<PendingOpState>, ApiError> {
        Ok(self.store.pending_op_state(op).await?)
    }

    /// Attempts every queued write for `account` that is due and that the drainer can
    /// dispatch, returning what became of each.
    ///
    /// **Call this when the device reconnects**, and after a sync. The engine runs no timer
    /// of its own: the reachability signal is the host's, and a poll from here would wake a
    /// dead network on a battery.
    ///
    /// A provider failure is not an error. It is recorded against its own op — retryable
    /// classes park for a later pass, the rest settle — and reported in the
    /// [`DrainReport`], so one bad recipient does not stop the rest of the queue going out.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the queue cannot be read or an outcome cannot be
    /// recorded.
    pub async fn drain_outbox<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<DrainReport, ApiError> {
        drain_outbox(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Everything still outstanding in `account`'s outbox, in enqueue order: what has
    /// not gone yet, what is being attempted, how many attempts each has had, and how
    /// the last one failed.
    ///
    /// A lease-free read, and the only way a host learns what it left behind: after a
    /// restart it holds none of the op ids
    /// [`pending_op_state`](Self::pending_op_state) answers about. Settled ops are
    /// excluded; they are kept as the idempotency record, not as outstanding work. **A send
    /// that settled `Failed` is the exception**: it is a message that has not gone, so it is
    /// listed, payload and all ([`queued_draft`]), until the host sends it again
    /// ([`retry_pending_op_now`](Self::retry_pending_op_now)) or withdraws it
    /// ([`cancel_pending_op`](Self::cancel_pending_op)).
    ///
    /// An op whose attempt died with its process is shown as its recovery will leave it, so
    /// nothing is listed as in flight under a worker that is gone.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn outbox(&self, account: &AccountId) -> Result<Vec<PendingOpRow>, ApiError> {
        Ok(self.store.list_pending_ops(account.clone()).await?)
    }

    /// Recovers the outbox ops the previous process left in flight, returning how many.
    ///
    /// Call it at start-up beside [`abandon_sync_leases`](Self::abandon_sync_leases), before
    /// any write or drain runs, when the previous process has ended. A send cut off before it
    /// handed its message over is retried at once; one cut off after may already have been
    /// delivered, so it awaits confirmation, where [`outbox`](Self::outbox) lists it, nothing
    /// runs it again, and [`confirm_pending_op`](Self::confirm_pending_op) or its copy
    /// appearing in a Sent mailbox resolves it. Every other write retries as a retryable
    /// failure does.
    ///
    /// Without it nothing is lost: every write path recovers an attempt once its lease has
    /// lapsed. It only saves waiting out that lease. Wrong about a worker that is still alive
    /// (a background task of the same app), it still delivers nothing twice.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn recover_interrupted_ops(&self) -> Result<usize, ApiError> {
        Ok(self.store.recover_interrupted_ops().await?)
    }

    /// Withdraws a queued op so it is never attempted, returning `None` when it was
    /// withdrawn and the reason when it could not be.
    ///
    /// Refusal is not failure: an op under a live lease may be mid-round-trip, and one
    /// awaiting confirmation may already have been delivered. Neither can be called back,
    /// so neither is withdrawn. A send that settled `Failed` is withdrawn here, which is how
    /// a host dismisses it.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn cancel_pending_op(
        &self,
        account: &AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>, ApiError> {
        Ok(self.store.cancel_pending_op(account.clone(), op).await?)
    }

    /// Clears a queued op's retry backoff so the next [`drain_outbox`](Self::drain_outbox)
    /// attempts it, returning `None` when it did and the reason when it could not.
    ///
    /// What a host wires to "Send now". The attempt count is not reset: one more attempt
    /// now is not a fresh bound. An op with no backoff to clear is already due, which is
    /// what the caller wanted, so that is `None` too. A send that settled `Failed` goes back
    /// in the queue ("Send again"). A send whose process died before it handed the message
    /// over is due at once; one that died after is refused as awaiting confirmation.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn retry_pending_op_now(
        &self,
        account: &AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>, ApiError> {
        Ok(self.store.retry_pending_op_now(account.clone(), op).await?)
    }

    /// Resolves a send awaiting confirmation with what the user knows, returning `None` when
    /// it took effect.
    ///
    /// The answer to the question a [`NeedsConfirmation`](PendingOpState::NeedsConfirmation)
    /// row asks: [`Delivered`](Confirmation::Delivered) settles it, and
    /// [`NotDelivered`](Confirmation::NotDelivered) puts it back in the queue, due at once, for
    /// the next [`drain_outbox`](Self::drain_outbox) to send. A send whose copy syncs into a
    /// Sent mailbox is confirmed without asking.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn confirm_pending_op(
        &self,
        account: &AccountId,
        op: PendingOpId,
        confirmation: Confirmation,
    ) -> Result<Option<OpRejection>, ApiError> {
        Ok(self
            .store
            .confirm_pending_op(account.clone(), op, confirmation)
            .await?)
    }
}

/// The message a queued send would deliver, decoded from an outbox row.
///
/// `None` for any row that is not a [`MailSubmit`](PendingOpKind::MailSubmit), including
/// one enqueued before the store recorded a kind. The payload is the driver's own
/// serialization of the [`Draft`], so this is the one supported way to read it back: a
/// host rendering an outbox needs the recipients and subject, and must not re-implement
/// the encoding to get them.
#[must_use]
pub fn queued_draft(row: &PendingOpRow) -> Option<Draft> {
    if row.kind != Some(PendingOpKind::MailSubmit) {
        return None;
    }
    serde_json::from_value(row.payload.clone()).ok()
}
