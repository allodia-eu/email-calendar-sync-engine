//! Sync contract types.
//!
//! These pure, async-free types are the contract shared between provider
//! adapters (which produce them), the sync orchestrator, and stores (which apply
//! them). They live in `engine-core` because both stores and sync consume them.
//! The async `Store` trait and its lease/fencing types belong to `engine-store`;
//! this module fixes only the data shapes: [`SyncScope`], the opaque
//! [`SyncState`] cursor, and the [`SyncUpdate`] delta/snapshot batch.

mod scope;
mod stamp;
mod state;
mod update;
mod widening;
mod window;

pub use scope::{JmapDataType, ObjectKind, SearchDomain, SyncScope};
pub use stamp::StampedCursor;
pub use state::SyncState;
pub use update::{Keyed, NoPatch, SyncObject, SyncUpdate};
pub use widening::{Band, Widening};
pub use window::{MailboxWindows, SyncWindow};
