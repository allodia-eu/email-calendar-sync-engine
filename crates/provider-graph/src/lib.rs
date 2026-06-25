//! `provider-graph` — the Microsoft Graph (v1.0) read/sync provider.
//!
//! Graph is the cloud-API counterpart to `provider-jmap` (OAuth bearer + JSON over
//! HTTP), but its mail sync is shaped like IMAP/CalDAV, not JMAP: message `delta`
//! is rooted at a **folder** (`/me/mailFolders/{id}/messages/delta`) with a
//! per-folder `deltaLink` cursor — there is **no** account-wide message delta — so
//! a [`GraphProvider`] is **bound to one folder** for email
//! ([`SyncScope::GraphFolder`](engine_core::sync::SyncScope)) while the folder list
//! syncs under the per-account
//! [`SyncScope::GraphFolderList`](engine_core::sync::SyncScope). The cross-folder
//! fan-out is the orchestrator's job, exactly as for `provider-imap`.
//!
//! Two real-behavior facts (captured from a live account; see
//! `tests/fixtures/README.md`) shape the design:
//!
//! - **Incremental `delta` returns *partial* objects** for changed items (only the
//!   changed properties plus `id`/`parentFolderId`), so the adapter **re-fetches**
//!   the full message for each changed id before emitting it — the engine applies
//!   whole objects, not property merges. The initial snapshot, by contrast,
//!   enumerates full objects.
//! - **Personal mail folders carry no `wellKnownName`** and their `displayName` is
//!   localized, so a [`MailboxRole`](engine_core::mail::MailboxRole) is resolved by
//!   matching a folder's id against the well-known-alias endpoints, never by name.
//!
//! # Layers
//!
//! - `error` — [`GraphError`] and its classification into the engine's
//!   `FailureClass` taxonomy.
//! - `json`/`normalize` — pure `serde_json::Value` → `Mailbox`/`Message`
//!   conversion, unit-tested against captured fixtures.
//! - `transport` — bearer HTTP behind the `GraphTransport` seam ([`GraphClient`]).
//! - `fetch` — the folder-list resolution and the message snapshot/delta + re-fetch
//!   paging.
//! - `provider` — [`GraphProvider`], the [`Provider`](engine_provider::Provider) impl.
//!
//! Tier-1 metadata only: like the other adapters, the raw MIME/body is fetched on
//! demand later, not materialized here.

mod error;
mod fetch;
mod json;
mod normalize;
mod provider;
mod transport;

#[cfg(test)]
mod test_support;

pub use error::GraphError;
pub use provider::GraphProvider;
pub use transport::GraphClient;
