//! Sync contract types.
//!
//! These pure, async-free types are the contract shared between provider
//! adapters (which produce them), the sync orchestrator, and stores (which apply
//! them). They live in `engine-core` because both stores and sync consume them.
//! The async `Store` trait and its lease/fencing types belong to `engine-store`;
//! this module fixes only the data shapes: [`SyncScope`], the opaque
//! [`SyncState`] cursor, and the [`SyncUpdate`] delta/snapshot batch.

mod scope;
mod state;
mod update;

pub use scope::{JmapDataType, SearchDomain, SyncScope};
pub use state::SyncState;
pub use update::SyncUpdate;
