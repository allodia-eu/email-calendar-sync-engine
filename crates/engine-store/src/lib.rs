//! `engine-store` — the store concurrency contract for the PIM sync engine.
//!
//! This crate owns the async [`Store`] trait, the lease and fencing types that
//! serialize writers, the [`ApplyBatch`]/[`DerivedWrite`] shapes committed
//! atomically per scope, the outbox op state machine, a reusable [`contract`]
//! test suite every store backend must pass, and an in-memory reference store
//! ([`mem`]).
//!
//! `store-and-sync.md` is authoritative for the concurrency model. The store is
//! **mechanical**: it performs no normalization, text extraction, or recurrence
//! expansion — pure `engine-core` code precomputes the derived rows before the
//! call. At-rest encryption is a *construction* detail of a concrete backend
//! (`store-sqlite`), not part of this contract: the trait is encryption-agnostic.

mod apply;
mod contact;
pub mod contract;
mod error;
mod lease;
pub mod mem;
mod outbox;
mod read;
mod settle;
mod source;
mod store;

pub use apply::{
    ApplyBatch, DerivedWrite, FtsField, FtsRow, OccurrenceRow, PendingReconciliation, PruneReport,
    SweepReport, SyncApplied, TzdataVersion,
};
pub use contact::{
    CachedContactPhoto, ContactPhotoCache, ContactPhotoFile, ContactSourceAvailability,
    ContactSourceSnapshot, ContactStore, PhotoCacheTtl,
};
pub use error::{Result, StoreError};
pub use lease::{
    Clock, FenceToken, LeaseRequest, ManualClock, OpLease, SyncClaim, SyncLease, WorkerId,
};
pub use outbox::{
    ClaimRejection, Confirmation, LeasedPendingOp, MAX_ATTEMPTS, OpRejection, PendingOpClaim,
    PendingOpRow, PendingOpState, retry_delay,
};
pub use read::{IndexRowCounts, MailListRow, MailSelector, SchemaStatus, StoreRead};
pub use settle::{
    Recorded, interrupted_outcome, record_outcome, settles_on_attempts, stays_listed,
};
pub use source::{MessageBodyStore, MessageSourceCache, SourcesDropped};
pub use store::Store;

/// The version of the engine's **normalization** — how providers decode wire data and
/// how `engine-core` projects it (subject charset decoding, header parsing, address
/// flattening, occurrence expansion, …). The store is a re-derivable cache of normalized
/// data, so when this logic changes, already-synced objects hold the *old* normalization
/// and an incremental delta sync will not refresh them. A backend stamps this version and,
/// on open, clears its sync cursors when it differs — forcing the next sync to re-snapshot
/// and re-normalize everything (`store-and-sync.md`).
///
/// **Bump it** whenever a change alters the bytes-to-object mapping in any provider or in
/// `engine-core`'s projection (e.g. the Windows-1252 subject fix), so existing stores
/// re-sync. A pure additive feature that does not change existing objects need not bump it.
///
/// History:
/// - `2`: the mail FTS `body` now folds in sender/recipient address text, and FTS terms are
///   prefix-matched; existing stores re-project to populate the new text.
/// - `3`: a message's thread id moved from `thread_id` to `thread`, which records whether the
///   provider assigned the id or the engine derived it (`threading.md`); a stored message
///   deserializes with no thread until the re-snapshot restores the provider's id.
/// - `4`: a Google **or Graph** event's `participants` no longer holds the organizer twice — the
///   `organizer` object merges into that person's `attendees[]` entry, one participant per address
///   with the roles unioned (`calendar-semantics.md`). A stored event keeps the duplicate pair,
///   whose synthesized `accepted` masks the participation status the account actually answered
///   (Google) or contradicts it with a placeholder `needs-action` (Graph), until the re-snapshot
///   re-projects it.
/// - `5`: a Graph event's `uid` is the iCalendar `UID` the organizer assigned, read from `uid`
///   rather than from `iCalUId` — Exchange's `PidLidGlobalObjectId` re-encoding of it, which wraps
///   an outside `UID` in a structure of its own (`graph.md`). A stored event keeps the re-encoding,
///   so a meeting organized outside Exchange carries one identity in the iMIP message that
///   announced it and another in the calendar that filed it, and scheduling reconciliation keying
///   on that value (RFC 5546 §2.1.5) matches neither, until the re-snapshot re-reads it.
/// - `6`: a mailbox carries the caller's [`MailboxAccess`](engine_core::mail::MailboxAccess), read
///   from JMAP `myRights` and the IMAP `MYRIGHTS` letters. A stored mailbox holds no rights and
///   loads as the `owner()` default — writable, deletable, renamable — which is wrong for every
///   folder whose owner granted less, such as each folder of a store shared read-only. A JMAP
///   folder list is a delta, so an unchanged mailbox would keep the default indefinitely; the
///   re-snapshot is what makes each one say what it really allows.
/// - `7`: an event's conference links became [`Conference`](engine_core::calendar::Conference)s:
///   one meeting with every way in it offers, the service it runs on and how it came to be on the
///   event, where the projection held bare join links. Google's dial-in numbers, codes and notes
///   and Graph's dial-in numbers and conference ID were not read at all, and an iCalendar
///   parameter's RFC 6868 escapes were not decoded. A stored event loads with no conferences until
///   the re-snapshot re-projects it.
/// - `8`: an event's meetings are also read from where no conference field holds them: the vendor
///   properties Google's and Microsoft's invitations carry, a location, and a meeting link in the
///   description. Graph's stand-in location for an online meeting ("Microsoft Teams Meeting") is no
///   longer a location. A stored event keeps the conferences it had until the re-snapshot
///   re-projects it.
/// - `9`: a detected meeting takes the fullest link any place writes for it, so a location's bare
///   Zoom link gives way to the description's one with its passcode.
/// - `10`: a link written inside another (Google Calendar's `google.com/url?q=` wrapper) is no
///   meeting of its own, and a Teams link is a meeting only under `/meet/` or `/l/meetup-join/`, so
///   an add-in's invitation no longer reads as two meetings.
/// - `11`: the re-sync a version change asks for reaches every calendar. Earlier changes cleared
///   the cursors and kept each calendar's collection tag, so a CalDAV calendar whose tag had not
///   moved was skipped as unchanged and kept the projection of whichever version first read it.
/// - `12`: a Gmail message's inbox category (`CATEGORY_*`), `CHAT` and superstar labels are no
///   longer memberships, nor mailboxes in the label list. The label list drops them on its next
///   read; a stored message keeps them as places until the re-snapshot re-reads it.
/// - `13`: a Graph event's description is its whole body, read as text, where an HTML body gave
///   only `bodyPreview`, cut at 255 characters. A notes edit written back from the preview lost the
///   rest and, on a Teams meeting, the meeting. A stored event keeps the preview until the
///   re-snapshot re-reads it.
/// - `14`: a Graph online meeting reads as removable: Exchange takes it off an event whose body is
///   written without Teams' block. A stored one keeps `removable: false` until the re-snapshot
///   re-reads it.
pub const NORMALIZER_VERSION: u32 = 14;
