//! The [`CalendarWrites`] half of the provider seam: creating, editing, answering and
//! deleting events.
//!
//! A supertrait of [`Provider`](crate::Provider) rather than more methods on it, so the
//! five verbs sit beside the types they take ([`EventDraft`], [`EventEdit`],
//! [`EventRsvp`], [`EventDeletion`]) instead of in a file that is otherwise about
//! syncing mail. Callers see no difference: a `P: Provider` exposes these exactly as
//! before, because the supertrait comes with the bound.
//!
//! Every verb defaults to rejecting, so an adapter that cannot write calendars states
//! that with an empty impl and a capability set that says the same
//! ([`Capabilities::calendar_writes`](crate::Capabilities::calendar_writes)). Reading
//! the capability first is the contract; the defaults are what make a caller that
//! forgets fail loudly rather than silently.

use async_trait::async_trait;
use engine_core::{calendar::Event, ids::AccountId};

// Named only by the doc links below, which rustdoc resolves against this module's scope.
#[allow(
    unused_imports,
    reason = "named by intra-doc links on the trait's methods"
)]
use crate::{Capabilities, ProviderError, RsvpControls};
use crate::{
    EventDeletion, EventDraft, EventEdit, EventRsvp, EventWrite, EventWriteReceipt, ProviderResult,
    error::unsupported,
};

/// The calendar-write verbs every adapter answers, rejecting by default.
///
/// See the module header for why this is its own trait.
#[async_trait]
pub trait CalendarWrites: Send + Sync {
    /// Creates a new event from an [`EventDraft`].
    ///
    /// The adapter serializes the draft in its own protocol — a document a CalDAV server
    /// stores, a JSCalendar object a JMAP server assigns an id to. The receipt names the
    /// [`EventId`](engine_core::ids::EventId) the create **resolved to**, which is the only
    /// place a server-assigning transport reveals it.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects, so a capability-checking caller never relies on it. Outbox-mediated by the
    /// caller (a durable pending op precedes this side effect); this method performs only
    /// the provider call.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. An event already existing at the target is a
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict); the default
    /// returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn create_event(
        &self,
        account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, draft);
        Err(unsupported("calendar writes"))
    }

    /// Applies an [`EventEdit`] to an already-stored event.
    ///
    /// `base` is the event **as the caller read it**, and it is load-bearing twice over: it
    /// carries the provider-native payload the patch is applied to (so an update never
    /// re-serializes the lossy projection — `calendar-semantics.md`), and the revision the
    /// write is guarded by, so a stale edit is refused rather than clobbering a newer one.
    /// Where the surgery happens differs by transport and is the adapter's business: CalDAV
    /// rewrites the stored `RawIcal` itself and `PUT`s it back, while JMAP hands the patch
    /// to a server whose update verb is already a patch.
    ///
    /// Whether the guard is actually enforced is **not** universal — see
    /// [`Capabilities::calendar_write_guard`].
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](CalendarWrites::create_event).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure — the server copy moved on —
    /// is [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict): refetch,
    /// re-apply the edit to the fresh base, resubmit; **never** blind-retry. A patch that
    /// would change the event's time *form* (silently converting a zoned event to a UTC
    /// instant, or an all-day event to a timed one) is rejected, not converted. The default
    /// returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn patch_event(
        &self,
        account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, base, edit);
        Err(unsupported("calendar writes"))
    }

    /// Replaces an event's whole stored document (CalDAV `PUT`).
    ///
    /// **Not** the neutral edit verb — [`patch_event`](CalendarWrites::patch_event) is. Only a
    /// document-oriented transport has this, and only an operation naturally expressed as a
    /// finished document should use it (today: the iMIP RSVP primitive). An adapter whose
    /// update verb is already a patch leaves this at the rejecting default *even though it
    /// advertises [`Capabilities::calendar_writes`]* — the capability covers the neutral
    /// spine, not this.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict); an adapter
    /// with no document verb returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState), as
    /// does the default.
    async fn put_event(
        &self,
        account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, write);
        Err(unsupported("whole-document calendar writes"))
    }

    /// Answers an invitation: sets **the account's own** participation status, and lets the
    /// server tell the organizer.
    ///
    /// Not an [`EventEdit`] of the attendee array, though it changes the same bytes: every
    /// transport routes scheduling through a distinct verb, so a patch would change the
    /// status and tell nobody. `base` is the event as the caller read it — the document the
    /// surgery runs over on a document transport, and the revision the write is guarded by.
    ///
    /// `rsvp.attendee` is the address the invitation **matched**, which on an aliased
    /// account is not the account's primary identity; an adapter uses it verbatim and never
    /// derives one ([`EventRsvp`]).
    ///
    /// Providers advertising [`Capabilities::calendar_rsvp`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](CalendarWrites::create_event).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) — refetch and
    /// re-answer, **never** blind-retry. An event with no `ATTENDEE` for that address, or a
    /// request for a control this transport does not honour (a `comment`, or
    /// `notify_organizer: false`, against [`RsvpControls`]), is
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) —
    /// refused rather than silently dropped. The default returns the same.
    async fn rsvp_event(
        &self,
        account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, base, rsvp);
        Err(unsupported("answering invitations"))
    }

    /// Deletes an event, or one occurrence of it, guarded by the revision the caller read.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](CalendarWrites::create_event).
    /// An event that is **already gone** is a success, not an error: the delete is
    /// idempotent, so a retry of one that already landed resolves cleanly.
    ///
    /// `base` is the event as the caller read it, when the caller has it. A
    /// [`Series`](crate::DeleteTarget::Series) delete needs nothing from it — the stored object
    /// goes whole — which is why it is optional. Removing one **occurrence** is a rewrite of
    /// the series on a document transport, so CalDAV needs the stored bytes and says so
    /// rather than guessing; the other three derive what they need from the deletion itself.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; a guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict), and the
    /// default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn delete_event(
        &self,
        account: &AccountId,
        base: Option<&Event>,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        let _ = (account, base, deletion);
        Err(unsupported("calendar writes"))
    }
}
