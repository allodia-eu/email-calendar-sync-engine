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
mod source;
mod store;

pub use apply::{
    ApplyBatch, DerivedWrite, FtsField, FtsRow, OccurrenceRow, PendingReconciliation, PruneReport,
    SyncApplied, TzdataVersion,
};
pub use contact::{
    CachedContactPhoto, ContactSourceAvailability, ContactSourceSnapshot, ContactStore,
};
pub use error::{Result, StoreError};
pub use lease::{
    Clock, FenceToken, LeaseRequest, ManualClock, OpLease, SyncClaim, SyncLease, WorkerId,
};
pub use outbox::{LeasedPendingOp, PendingOpState};
pub use source::{MessageBodyStore, MessageSourceCache};
pub use store::{IndexRowCounts, MailListRow, MailSelector, SchemaStatus, Store, StoreRead};

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
pub const NORMALIZER_VERSION: u32 = 4;
