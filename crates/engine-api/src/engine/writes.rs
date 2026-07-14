//! The outbox-mediated **mail** writes on `Engine` — submission and edits — plus the
//! pending-op state poll every write (mail and calendar alike) is observed through. The
//! calendar writes live in `calendar_writes`, which additionally reconciles the store.

use engine_core::{ids::AccountId, write::PendingOpId};
use engine_provider::{Draft, MailEdit, Provider};
use engine_store::{PendingOpState, StoreRead};
use engine_sync::{MailEditOutcome, SubmitOutcome, edit_mail, submit_mail};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Submits `draft` for one account through the durable outbox: the draft is
    /// recorded as a pending op (idempotent by its `Message-ID`) **before** the
    /// provider send, so a crash or an ambiguous failure never loses or double-sends
    /// it (`north-star.md` Write Contract). Returns the sent message's key, its
    /// `Message-ID`, and the op id — pollable via [`Engine::pending_op_state`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the send fails: the op is first recorded
    /// `Failed` (with the failure class), or `NeedsConfirmation` for an ambiguous
    /// post-`DATA` SMTP loss — the outbox never blind-retries — and the error then
    /// returns. A store failure also surfaces as [`ApiError::Sync`].
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

    /// Applies a [`MailEdit`] to one of the account's messages through the durable
    /// outbox — mark-read/flag (`SetKeywords`), move to another folder
    /// (`MoveTo` — also the mechanism behind a Trash "delete", the host resolving the
    /// Trash mailbox), or permanent delete (`Delete`). The edit is recorded as a
    /// pending op (idempotent by `idempotency`) **before** the provider side effect,
    /// so a crash never loses it (`north-star.md` Write Contract). `idempotency` must
    /// be **unique per edit intent** — deriving it only from the target message would
    /// wrongly collapse mark-read then mark-unread into one op. Returns the resolved
    /// message key and the op id (pollable via [`Engine::pending_op_state`]).
    ///
    /// The next [`Engine::sync_mail`] reconciles the local rows to the new server
    /// state (a periodic snapshot, since IMAP deltas do not carry flag/expunge
    /// changes — `imap-smtp.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the edit fails: the op is first recorded
    /// `Failed` (a stale-target `Conflict` — e.g. an IMAP UID under a changed
    /// `UIDVALIDITY` — means re-sync then retry), and the error then returns. A store
    /// failure also surfaces as [`ApiError::Sync`].
    pub async fn edit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        edit: &MailEdit,
    ) -> Result<MailEditOutcome, ApiError> {
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
}
