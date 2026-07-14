//! The outbox-mediated writes on `Engine` — mail submission and edits, calendar
//! event writes and deletes — plus the pending-op state poll they are observed
//! through.

use engine_core::{calendar::Event, ids::AccountId, write::PendingOpId};
use engine_provider::{
    Draft, EventDeletion, EventDraft, EventPatch, EventWrite, MailEdit, PatchTarget, Provider,
};
use engine_store::{PendingOpState, StoreRead};
use engine_sync::{
    CalendarWriteOutcome, MailEditOutcome, SubmitOutcome, create_calendar_event,
    delete_calendar_event, edit_mail, patch_calendar_event, put_calendar_document, submit_mail,
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

    /// Creates a calendar event through the durable outbox.
    ///
    /// The host states the event it wants ([`EventDraft`] — a title, a start, a calendar)
    /// and the **adapter** serializes it: CalDAV builds an iCalendar document and `PUT`s it
    /// under `If-None-Match: *`; JMAP posts a JSCalendar object and the server assigns the
    /// id. So this call is the same on every transport, and the host never assembles a
    /// protocol payload.
    ///
    /// The create is recorded as a pending op (idempotent by `idempotency`, serialized on
    /// the event's `UID` so two writes to one event never race) **before** the provider side
    /// effect, so a crash never loses it (`north-star.md` Write Contract). `idempotency`
    /// must be **unique per write intent**. Returns the [`EventId`](engine_core::ids::EventId)
    /// the create resolved to — which on a server-assigning transport is revealed nowhere
    /// else — the new revision if the server reported one, and the op id (pollable via
    /// [`Engine::pending_op_state`]).
    ///
    /// The next [`Engine::sync_calendar`] reconciles the local rows to the server's copy.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the create fails: an event already existing at the
    /// target is recorded `Failed` with a `Conflict` class — re-sync, do not blind-retry —
    /// and the error then returns. A store failure also surfaces as [`ApiError::Sync`].
    pub async fn create_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        draft: &EventDraft,
    ) -> Result<CalendarWriteOutcome, ApiError> {
        create_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            draft,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Edits a stored calendar event through the durable outbox.
    ///
    /// `base` is the event **as read from the store**, and `target` says whether the edit
    /// lands on the whole series or on one occurrence — a question with no safe default, so
    /// the product UI must ask (`calendar-semantics.md`). The adapter applies the patch in
    /// its own protocol: CalDAV rewrites only the touched lines of the stored iCalendar and
    /// `PUT`s it back under `If-Match`, JMAP hands a JSON-pointer patch to a server whose
    /// update verb is already a patch. Either way the properties the engine does not model —
    /// the alarms, the embedded zone, another client's `X-` properties — survive, because
    /// the document is **never** rebuilt from the lossy projection.
    ///
    /// The edit is guarded by the revision `base` was read at. **Whether the server enforces
    /// that guard is not universal**: check
    /// [`Capabilities::calendar_write_guard`](engine_provider::Capabilities::calendar_write_guard).
    /// Under [`WriteGuard::Absent`](engine_provider::WriteGuard) a stale edit silently wins,
    /// so a successful write does not mean no concurrent edit was lost.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the patch fails. A stale guard is recorded `Failed`
    /// with a `Conflict` class — re-sync, re-apply the edit to the fresh copy, resubmit;
    /// **never** blind-retry. A patch that would silently convert the event's time form (a
    /// zoned event to a UTC instant, an all-day event to a timed one) is rejected outright.
    /// A store failure also surfaces as [`ApiError::Sync`].
    pub async fn patch_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        base: &Event,
        target: PatchTarget,
        patch: EventPatch,
    ) -> Result<CalendarWriteOutcome, ApiError> {
        patch_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            base,
            target,
            patch,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Replaces a calendar event's whole stored document through the durable outbox.
    ///
    /// **Not the way to edit an event** — [`patch_calendar_event`](Self::patch_calendar_event)
    /// is. This is the escape hatch for operations that are naturally a finished document
    /// rather than a property patch, today the iMIP RSVP primitive
    /// (`provider_caldav::imip::set_my_partstat`), and only a document-oriented adapter
    /// supports it at all.
    ///
    /// # Errors
    ///
    /// As [`patch_calendar_event`](Self::patch_calendar_event), plus an `InvalidState` from
    /// an adapter with no whole-document write verb (JMAP).
    pub async fn put_calendar_document<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        write: &EventWrite,
    ) -> Result<CalendarWriteOutcome, ApiError> {
        put_calendar_document(
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

    /// Deletes a calendar event through the durable outbox, guarded by the revision the
    /// caller read it at.
    ///
    /// Recorded as a pending op (idempotent by `idempotency`, serialized on the event's
    /// `UID`, which the deletion carries) **before** the provider side effect, so a crash never
    /// loses it (`north-star.md` Write Contract). `idempotency` must be **unique per delete
    /// intent**. An already-gone event resolves as success (the delete is idempotent). Returns
    /// the op id (pollable via [`Engine::pending_op_state`]); the next
    /// [`Engine::sync_calendar`] tombstones the local row.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the delete fails: a stale guard is recorded `Failed`
    /// with a `Conflict` class — re-sync, then retry — and the error then returns. A store
    /// failure also surfaces as [`ApiError::Sync`].
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
