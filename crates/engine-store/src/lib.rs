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
pub mod contract;
mod error;
mod lease;
pub mod mem;
mod outbox;
mod store;

pub use apply::{
    ApplyBatch, DerivedWrite, FtsField, FtsRow, OccurrenceRow, PendingReconciliation,
    StorableObject, SyncApplied, TzdataVersion,
};
pub use error::{Result, StoreError};
pub use lease::{
    Clock, FenceToken, LeaseRequest, ManualClock, OpLease, SyncClaim, SyncLease, WorkerId,
};
pub use outbox::{LeasedPendingOp, PendingOpState};
pub use store::{IndexRowCounts, Store, StoreRead};
