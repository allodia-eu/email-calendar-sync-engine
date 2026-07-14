//! The outbox-mediated **calendar** writes on `Engine` — create, patch, replace-document
//! (RSVP), delete — each of which reconciles the store to the server's copy before it
//! returns.
//!
//! # Read-your-writes
//!
//! A calendar write returns a receipt, not a document: a CalDAV `PUT` answers with an
//! `ETag` and no body, a JMAP `CalendarEvent/set` with an id and no object. The driver
//! underneath these methods therefore leaves the store holding the **pre-write** event —
//! the old projection, the old `raw_ical`, the old revision (issue #65). A host that then
//! re-read the store would see the event it just replaced, and — worse — would build its
//! *next* edit's guard from the **superseded** revision and be refused with a `412` on a
//! write that should have succeeded.
//!
//! So every write here runs [`engine_sync::reconcile_calendar_events`] — an event-scope
//! delta, one round trip on either transport — the moment the write lands. The store then
//! holds what the **server** holds (never the bytes we sent: see the `engine_sync::calendar`
//! module docs for why storing our own copy would mask a server that silently dropped a
//! property), a delete is tombstoned locally instead of lingering until the next sync, and
//! an edit that moved the event moves its occurrence rows.
//!
//! The reconcile is a *local* step after a write the server has already accepted, so it
//! can never fail the write: it is reported as [`Reconciled`], not as an error.

use engine_core::{calendar::Event, ids::AccountId, write::PendingOpId};
use engine_provider::{EventDeletion, EventDraft, EventPatch, EventWrite, PatchTarget, Provider};
use engine_recurrence::Horizon;
use engine_sync::{
    CalendarWriteOutcome, EventSyncReport, create_calendar_event, delete_calendar_event,
    patch_calendar_event, put_calendar_document,
};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine, TimeZoneId};

/// A calendar write that **landed on the server**, and what the store now holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarWrite {
    /// The write itself: the durable op, the event it resolved to, its `UID`, and the
    /// revision the receipt carried.
    pub write: CalendarWriteOutcome,
    /// Whether the store was brought up to the server's copy of it.
    pub reconciled: Reconciled,
}

/// A calendar delete that **landed on the server**, and what the store now holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarDelete {
    /// The durable op that recorded the delete (pollable via
    /// [`Engine::pending_op_state`]).
    pub op: PendingOpId,
    /// Whether the store was brought up to the server's copy — here, the event's absence.
    pub reconciled: Reconciled,
}

/// Whether the store holds the server's copy of a write that already succeeded.
///
/// **A write that did not reconcile is still a write.** Anything but
/// [`Applied`](Reconciled::Applied) means only the *local* copy is stale — the server has
/// the change. Never re-issue the write to "fix" it: that would write twice. Re-read it
/// instead, with [`Engine::reconcile_calendar_events`] or the next
/// [`Engine::sync_calendar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// The post-write delta ran: the store holds the server's canonical event — or, for a
    /// delete, no event at all.
    Applied(EventSyncReport),
    /// Another sync holds the account's event scope, so the delta could not run. The store
    /// still holds the pre-write copy; that sync, or the next one, picks the change up.
    Busy,
    /// The delta itself failed (the provider or the store), with the reason. The store
    /// still holds the pre-write copy until something re-reads it.
    Failed(String),
}

impl Engine {
    /// Creates a calendar event through the durable outbox, then reconciles the store to
    /// the server's copy of it.
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
    /// must be **unique per write intent**. Returns the
    /// [`EventId`](engine_core::ids::EventId) the create resolved to — which on a
    /// server-assigning transport is revealed nowhere else — the new revision if the server
    /// reported one, the op id (pollable via [`Engine::pending_op_state`]), and whether the
    /// store now holds the event ([`Reconciled`]).
    ///
    /// `horizon`/`host_zone` are the reconcile's, and mean what they do on
    /// [`sync_calendar`](Engine::sync_calendar): the window the new event's occurrences are
    /// materialized over, and the zone a floating time resolves through.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the create fails: an event already existing at the
    /// target is recorded `Failed` with a `Conflict` class — re-sync, do not blind-retry —
    /// and the error then returns. A store failure also surfaces as [`ApiError::Sync`]. A
    /// failure to *reconcile* is **not** an error (the create landed): it is reported in
    /// [`CalendarWrite::reconciled`].
    pub async fn create_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        draft: &EventDraft,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<CalendarWrite, ApiError> {
        let write = create_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            draft,
        )
        .await
        .map_err(map_sync_error)?;
        Ok(self
            .reconciling(provider, account, write, horizon, host_zone)
            .await)
    }

    /// Edits a stored calendar event through the durable outbox, then reconciles the store
    /// to the server's copy of it.
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
    /// Because the write reconciles, a host may edit the same event twice by re-reading it
    /// from the store in between: the second edit's guard is the revision the *server*
    /// reported, not the superseded one it wrote over.
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
    /// A store failure also surfaces as [`ApiError::Sync`]. A failure to *reconcile* is
    /// **not** an error (the patch landed): it is reported in [`CalendarWrite::reconciled`].
    #[allow(
        clippy::too_many_arguments,
        reason = "the write's base and intent, the outbox's key, and the reconcile's window \
                  are three distinct concerns; folding them into one struct would rename the \
                  problem, not solve it"
    )]
    pub async fn patch_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        base: &Event,
        target: PatchTarget,
        patch: EventPatch,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<CalendarWrite, ApiError> {
        let write = patch_calendar_event(
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
        .map_err(map_sync_error)?;
        Ok(self
            .reconciling(provider, account, write, horizon, host_zone)
            .await)
    }

    /// Replaces a calendar event's whole stored document through the durable outbox, then
    /// reconciles the store to the server's copy of it.
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
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<CalendarWrite, ApiError> {
        let outcome = put_calendar_document(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            write,
        )
        .await
        .map_err(map_sync_error)?;
        Ok(self
            .reconciling(provider, account, outcome, horizon, host_zone)
            .await)
    }

    /// Deletes a calendar event through the durable outbox, guarded by the revision the
    /// caller read it at, then reconciles the store — which **tombstones the local row**,
    /// so the event is gone from every read (and every occurrence row) as soon as this
    /// returns.
    ///
    /// Recorded as a pending op (idempotent by `idempotency`, serialized on the event's
    /// `UID`, which the deletion carries) **before** the provider side effect, so a crash
    /// never loses it (`north-star.md` Write Contract). `idempotency` must be **unique per
    /// delete intent**. An already-gone event resolves as success (the delete is
    /// idempotent).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the delete fails: a stale guard is recorded `Failed`
    /// with a `Conflict` class — re-sync, then retry — and the error then returns. A store
    /// failure also surfaces as [`ApiError::Sync`]. A failure to *reconcile* is **not** an
    /// error (the delete landed): it is reported in [`CalendarDelete::reconciled`].
    pub async fn delete_calendar_event<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        idempotency: &str,
        deletion: &EventDeletion,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<CalendarDelete, ApiError> {
        let op = delete_calendar_event(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            idempotency,
            deletion,
        )
        .await
        .map_err(map_sync_error)?;
        Ok(CalendarDelete {
            op,
            reconciled: self
                .reconcile_after_write(provider, account, horizon, host_zone)
                .await,
        })
    }

    /// Pairs a landed write with the reconcile that follows it.
    async fn reconciling<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        write: CalendarWriteOutcome,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> CalendarWrite {
        CalendarWrite {
            reconciled: self
                .reconcile_after_write(provider, account, horizon, host_zone)
                .await,
            write,
        }
    }

    /// Re-reads the account's events after a write the server has **already accepted**.
    ///
    /// Returns [`Reconciled`] rather than a `Result` on purpose: the write is committed and
    /// durable, so a failure here means the local copy is stale — never that the write
    /// failed. Surfacing it as an error would invite a host to re-issue a write the server
    /// has already applied.
    async fn reconcile_after_write<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Reconciled {
        match self
            .reconcile_calendar_events(provider, account, horizon, host_zone)
            .await
        {
            Ok(report) => Reconciled::Applied(report),
            Err(ApiError::Busy) => Reconciled::Busy,
            Err(err) => Reconciled::Failed(err.to_string()),
        }
    }
}
