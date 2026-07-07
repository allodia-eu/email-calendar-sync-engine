//! The outbox-mediated writes on `Engine` — mail submission and edits, calendar
//! event writes and deletes — plus the pending-op state poll they are observed
//! through.

use engine_core::{ids::AccountId, write::PendingOpId};
use engine_provider::{Draft, EventDeletion, EventWrite, MailEdit, Provider};
use engine_store::{PendingOpState, StoreRead};
use engine_sync::{
    CalendarWriteOutcome, MailEditOutcome, SubmitOutcome, delete_calendar_event, edit_mail,
    submit_mail, write_calendar_event,
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

    /// Creates or replaces a calendar event through the durable outbox — a
    /// conditional CalDAV `PUT` of the iCalendar body in `write` (a create carries
    /// an `If-None-Match: *` guard, an update an `If-Match: <etag>` one). The write
    /// is recorded as a pending op (idempotent by `idempotency`, serialized on the
    /// resource href so two writes to one event never race) **before** the provider
    /// side effect, so a crash never loses it (`north-star.md` Write Contract;
    /// `caldav.md`). `idempotency` must be **unique per write intent** — deriving it
    /// only from the target href would wrongly collapse two distinct edits of one
    /// event into one op. The body is built by the host (e.g. with
    /// `provider_caldav::build_event_ical` for a create) or round-tripped from the
    /// stored raw for an update, never re-serialized from the lossy projection
    /// (`calendar-semantics.md`). Returns the
    /// resource key, the new `ETag` if the server returned one, and the op id
    /// (pollable via [`Engine::pending_op_state`]).
    ///
    /// The next [`Engine::sync_calendar`] reconciles the local rows to the new server
    /// revision (a `sync-collection` delta carrying the fresh `ETag` when the `PUT`
    /// response omitted it).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the write fails: a `412` precondition failure
    /// (the resource already exists for a create, or its `ETag` moved for an update)
    /// is recorded `Failed` with a `Conflict` class — refetch and merge, never blind
    /// retry — and the error then returns. A store failure also surfaces as
    /// [`ApiError::Sync`].
    pub async fn write_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        write: &EventWrite,
    ) -> Result<CalendarWriteOutcome, ApiError> {
        write_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            write,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Deletes a calendar event through the durable outbox — a CalDAV `DELETE` of the
    /// resource in `deletion` (optionally guarded by `If-Match: <etag>`). The delete
    /// is recorded as a pending op (idempotent by `idempotency`, serialized on the
    /// resource href) **before** the provider side effect, so a crash never loses it
    /// (`north-star.md` Write Contract; `caldav.md`). `idempotency` must be **unique
    /// per delete intent**. An already-gone resource resolves as success (`DELETE` is
    /// idempotent, RFC 7231 §4.3.5). Returns the op id (pollable via
    /// [`Engine::pending_op_state`]); the next [`Engine::sync_calendar`] tombstones
    /// the local row.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the delete fails: a `412` (the guarded `ETag`
    /// moved) is recorded `Failed` with a `Conflict` class — refetch and retry — and
    /// the error then returns. A store failure also surfaces as [`ApiError::Sync`].
    pub async fn delete_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        deletion: &EventDeletion,
    ) -> Result<PendingOpId, ApiError> {
        delete_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            deletion,
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
