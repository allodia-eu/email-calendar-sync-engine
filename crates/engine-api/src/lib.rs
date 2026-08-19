//! `engine-api` — the stable facade hosts consume (`north-star.md`).
//!
//! Every host — mobile (UniFFI), desktop/daemon (the C ABI), the CLI, and server
//! adapters — drives the engine through this one crate instead of wiring
//! `engine-store`, `engine-sync`, the providers, and a clock together itself. The
//! facade owns that composition: an [`Engine`] holds one durable
//! [`SqliteStore`](store_sqlite::SqliteStore) (the first store; other backends are
//! host adapters) driven by the host wall clock (`SystemClock`), and exposes
//! high-level operations over it.
//!
//! # Scope of this slice
//!
//! Step 6 of the build order lands in small, tested slices. These cover **store
//! lifecycle** ([`Engine::open`], [`Engine::open_in_memory`]), **provider-driven
//! sync** ([`Engine::sync_mail`], [`Engine::sync_calendar`]), **per-account
//! search** ([`Engine::search_mail`], [`Engine::search_calendar`]),
//! **outbox-mediated mail submission** ([`Engine::submit_mail`],
//! [`Engine::pending_op_state`]), **streaming mail sync**
//! ([`Engine::sync_mail`], which streams each folder and fans them out itself, for
//! concurrent cross-folder sync), and **outbox-mediated calendar writes**
//! ([`Engine::create_calendar_event`], [`Engine::patch_calendar_event`],
//! [`Engine::delete_calendar_event`]) — which carry *intent*, so the same call drives
//! CalDAV and JMAP and the host never assembles a protocol payload, and which
//! **reconcile the store to the server's copy before they return**, so a host can
//! re-read what it just wrote (and guard its next edit on the revision the server
//! actually reported). The language bindings themselves are a deliberate follow-up slice.
//!
//! # Shape
//!
//! - The store is concrete ([`SqliteStore`](store_sqlite::SqliteStore)): SQLite is the engine's
//!   first store, and the search and other conveniences live on it, not on the
//!   `engine_store::Store` trait.
//! - Sync is **generic over [`Provider`]**, so the facade stays provider-agnostic — a host passes a
//!   `provider-jmap`, `provider-imap`, or `provider-caldav` adapter and the facade never switches
//!   on protocol.
//! - The wall clock lives here, not in `engine-store`, so the store keeps a single injected time
//!   seam and never reads the system clock itself (`north-star.md`).
//!
//! The types that appear in the facade's own signatures are re-exported, so a host
//! can depend on `engine-api` alone and still name everything it needs to call it.

mod body;
mod clock;
mod engine;
mod scheduling;

pub use engine::{
    CalendarDelete, CalendarWrite, ContactDelete, ContactReconciled, ContactWrite, Engine,
    PeoplePage, PeopleQuery, RecipientSuggestions, Reconciled,
};
// Re-exports of the types this facade's signatures mention, so hosts depend on
// `engine-api` alone (the providers themselves still come from the adapter crates).
pub use engine_core::calendar::{
    Calendar, Event, EventKind, EventStatus, FreeBusyStatus, Location, Participant,
    ParticipantKind, ParticipantRole, ParticipationStatus, Privacy, VirtualLocation,
};
// The inbound-scheduling (iTIP/iMIP) layer. `Engine::message_scheduling` returns a
// `SchedulingMessage`, so without these a host could not name what it received — and the
// product rule that decides whether to offer an RSVP is written over `ScheduleMethod` plus
// `ParticipationStatus`, both of which therefore have to be nameable here.
//
// `addresses_match`/`normalize_address` are exported because matching an `ATTENDEE` against
// the account's own address set is the *same* comparison the trust decision makes, and a
// second implementation of it in a host would be a second set of bugs (iCalendar cases
// domains freely and writes `mailto:` inconsistently).
pub use engine_core::scheduling::{
    ImipTrust, ImipUntrusted, InstanceKey, Revision, ScheduleAction, ScheduleMethod,
    SchedulingMessage, SchedulingMode, addresses_match, apply_reply, cancel, evaluate_imip_trust,
    find_calendar_part, normalize_address, reconcile,
};
pub use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactField, ContactFieldSet, ContactKind,
        ContactPatch, ContactResource, ContactSourceClass, FieldPatch,
    },
    coverage::SearchCoverage,
    // Every id that appears on a type this facade returns. `Event` carries an `EventId`, a `Uid`
    // and `Memberships<CalendarId>`; without these a host holding an `Event` cannot name the
    // fields it can already read, and would have to depend on `engine-core` directly — the
    // reach-around this re-export block exists to prevent.
    ids::{
        AccountId, AddressBookId, CalendarId, ContactId, EventId, MessageIdHeader, PersonId,
        ProviderKey, ThreadId, Uid,
    },
    mail::{
        AttachmentPartId, EmailAddress, InlinePart, Keyword, Mailbox, MailboxRole, Message,
        MessageAttachment, MessageAttachmentContent, MessageBody, SystemKeyword, ThreadProvenance,
        ThreadRef,
    },
    // The set-of-containers type an `Event`'s `calendars` and a `Message`'s mailbox memberships
    // are expressed in.
    membership::Memberships,
    // The unified-people and recipient-history types. `PeoplePage` and `RecipientSuggestions`
    // are returned by this facade and are *made of* these, so without them a host cannot name
    // what it just received — it would have to depend on `engine-core` directly, which is the
    // reach-around this re-export block exists to prevent.
    people::{CanonicalEmail, PeopleSnapshot, Person, PersonSource, PersonSourceId, SourcedValue},
    // The event's verbatim iCalendar, preserved beside the lossy projection. An RSVP is
    // applied *to the raw* by targeted patch, so a host that inspects or round-trips a
    // scheduling payload needs to be able to name it.
    raw::RawIcal,
    recipient::{RecipientCoverage, RecipientInteraction, RecipientSuggestion},
    sync::{SyncScope, SyncWindow},
    // `CalendarDateTime` is the type of `Event::start` and `Event::recurrence_id`, and `Duration`
    // the type of `Event::duration` — both public fields on a type this facade returns.
    time::{
        CalendarDate, CalendarDateTime, Duration, LocalDateTime, TimeZoneId, UtcDateTime,
        resolve_zone_name,
    },
    write::PendingOpId,
};
pub use engine_core::{mail::MailFlags, search_index::MailRow};
pub use engine_provider::{
    Capabilities, ContactDestination, ContactPhoto, ContactsProvider, ContentIdHeader, Draft,
    DraftAttachment, DraftAttachmentDisposition, DraftCalendar, EventDeletion, EventDraft,
    EventEdit, EventPatch, EventRsvp, EventWrite, EventWriteReceipt, MailEdit, MailEditReceipt,
    PatchTarget, Provider, ReplyDelivery, RsvpControls, RsvpResponse, SentCopy, SubmissionReceipt,
    TextEdit, WriteGuard, WritePrecondition,
};
pub use engine_recurrence::{
    ExpandError, Horizon, available_zones, day_bounds_utc, is_supported_zone, resolve_instant,
    resolve_instant_in, to_local,
};
pub use engine_search::{ParseError, SearchHit, SearchResults};
use engine_store::StoreError;
pub use engine_store::{
    MailListRow, OccurrenceRow, PendingOpState, PruneReport, SchemaStatus, SweepReport,
    SyncApplied, TzdataVersion,
};
pub use engine_sync::{
    AccountProgress, CalendarSyncReport, CalendarWriteOutcome, ContactReconcileReport,
    ContactSourceReport, ContactSyncReport, ContactWriteOutcome, EventSyncReport, FolderSync,
    HorizonExpansion, IgnoreCommits, MailEditOutcome, MailSyncReport, PeopleRebuildReport,
    ProgressSnapshot, StreamTuning, SubmitOutcome, SyncCommit, SyncError, SyncObserver, SyncTiming,
    ThreadRebuildReport, UnexpandableEvent,
};
pub use scheduling::InboundScheduling;

/// An error from an [`Engine`] operation.
///
/// [`Store`](ApiError::Store) and [`Sync`](ApiError::Sync) wrap the underlying
/// engine error unchanged, so the original `source()` chain (provider failure
/// class, store backend detail) stays inspectable. [`Busy`](ApiError::Busy) is
/// split out from a sync failure so a host can tell a benign scope-contention race
/// — safe to retry — from a real error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// Opening or migrating the store failed.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    /// A sync cycle failed: a provider fetch or the store apply.
    #[error("sync error: {0}")]
    Sync(#[source] SyncError),
    /// The search query string was malformed — an unbalanced quote, an empty
    /// operator value, or a bad `before:`/`after:` date or boolean.
    #[error("query error: {0}")]
    Query(#[from] ParseError),
    /// Another sync already holds this account's scope; the call did nothing and
    /// can be retried once the in-flight sync finishes. Raised when a concurrent
    /// sync of the same `(account, scope)` makes the store return the retryable
    /// `ScopeHeld` — the sync loop surfaces it rather than waiting for the lease.
    #[error("scope is busy: another sync is in progress; retry shortly")]
    Busy,
    /// Host input or a stale/mismatched opaque cursor was rejected before I/O.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl ApiError {
    /// Whether this failure is a provider **conflict** — the provider's state moved
    /// underneath the operation (an IMAP `UIDVALIDITY` renumbering, a stale or
    /// expunged target), classified
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict). The
    /// documented recovery is *re-sync the affected scope, then retry* (e.g. the
    /// [`Engine::message_body`] error contract); this accessor lets a host automate
    /// that recovery without parsing error text.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            Self::Sync(SyncError::Provider(err))
                if err.class() == engine_core::error::FailureClass::Conflict
        )
    }
}

#[cfg(test)]
mod error_tests {
    use engine_provider::ProviderError;

    use super::*;

    #[test]
    fn conflict_classification_is_exposed_to_hosts() {
        let conflict = ApiError::Sync(SyncError::Provider(ProviderError::conflict(
            "UIDVALIDITY changed for X: re-sync before retrying",
        )));
        assert!(conflict.is_conflict());

        let transient = ApiError::Sync(SyncError::Provider(ProviderError::retryable("blip")));
        assert!(!transient.is_conflict(), "retryable is not a conflict");
        assert!(!ApiError::Busy.is_conflict(), "Busy is not a conflict");
    }
}
