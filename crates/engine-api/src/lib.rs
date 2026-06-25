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
//! [`Engine::pending_op_state`]), and **streaming mail sync**
//! ([`Engine::sync_mail_streamed`]). Calendar writes and the language bindings
//! themselves are deliberate follow-up slices.
//!
//! # Shape
//!
//! - The store is concrete ([`SqliteStore`](store_sqlite::SqliteStore)): SQLite is
//!   the engine's first store, and the search and other conveniences live on it,
//!   not on the `engine_store::Store` trait.
//! - Sync is **generic over [`Provider`]**, so the facade stays provider-agnostic —
//!   a host passes a `provider-jmap`, `provider-imap`, or `provider-caldav`
//!   adapter and the facade never switches on protocol.
//! - The wall clock lives here, not in `engine-store`, so the store keeps a single
//!   injected time seam and never reads the system clock itself (`north-star.md`).
//!
//! The types that appear in the facade's own signatures are re-exported, so a host
//! can depend on `engine-api` alone and still name everything it needs to call it.

mod clock;
mod engine;

use engine_store::StoreError;
use engine_sync::SyncError;

pub use engine::Engine;

// Re-exports of the types this facade's signatures mention, so hosts depend on
// `engine-api` alone (the providers themselves still come from the adapter crates).
pub use engine_core::calendar::{Calendar, Event};
pub use engine_core::coverage::SearchCoverage;
pub use engine_core::ids::{AccountId, MessageIdHeader, ProviderKey};
pub use engine_core::mail::{EmailAddress, Mailbox, MailboxRole, Message, SystemKeyword};
pub use engine_core::time::TimeZoneId;
pub use engine_core::write::PendingOpId;
pub use engine_provider::{Draft, MailEdit, MailEditReceipt, Provider};
pub use engine_recurrence::Horizon;
pub use engine_search::{ParseError, SearchHit, SearchResults};
pub use engine_store::PendingOpState;
pub use engine_sync::{
    CalendarSyncReport, MailEditOutcome, MailSyncReport, ProgressSink, SubmitOutcome, SyncProgress,
    ThreadDeriveReport,
};

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
}
