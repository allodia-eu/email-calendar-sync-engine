//! `engine-provider` — the provider/transport trait surface.
//!
//! A provider adapter turns remote mail, calendar, and contact state into the
//! engine's normalized shapes. This crate defines the **small**
//! contract every adapter implements so the sync orchestrator and stores never
//! switch on provider kind (`providers.md`):
//!
//! - return a normalized [`SyncUpdate`](engine_core::sync::SyncUpdate) plus an opaque next cursor,
//!   bundled as [`ScopeSync`] — or, for a responsive UI, one [`SyncPage`] at a time;
//! - expose what it can do, and what its transport negotiated, via one [`ConnectionInfo`];
//! - classify failures through [`ProviderError`] (the engine-neutral
//!   [`FailureClass`](engine_core::error::FailureClass) taxonomy);
//! - signal delta-vs-snapshot (carried inside the [`SyncUpdate`](engine_core::sync::SyncUpdate)
//!   itself).
//!
//! [`ConnectionInfo`] reports the *outcome* of a connect. The phase itself — the
//! well-known redirects, the TLS handshake, authentication, endpoint discovery — is
//! observable through [`ConnectStep`] and [`ConnectObserver`], which an adapter's
//! config carries.
//!
//! A credential often reaches more than its own mailbox. [`SharedMailboxes`] and
//! [`SharedMailbox`] are the discovery seam for that (`shared`): they answer *which
//! stores can I open*, and stop there — a discovered store becomes an ordinary account
//! with the same credential, so no scope, cursor, or account kind is added.
//!
//! The trait — [`Provider`], in the `provider` module — is deliberately **shaped by
//! JMAP** and kept minimal: it covers the mail spine (mailboxes + email), calendar,
//! submission, and contact contracts. It depends only on `engine-core`; network access
//! and an async runtime live in the concrete provider crates (`provider-jmap`,
//! plus `provider-imap`/`provider-smtp`/`provider-caldav`). `engine-sync` drives
//! the provider-neutral scopes.

mod boxed;
mod calendar_write;
mod capability;
mod connect_observer;
mod connection;
mod contact;
mod error;
mod mail_edit;
mod page;
mod provider;
mod shared;
mod stream;
mod submit;
mod sync;
mod watch;

pub use calendar_write::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventWrite, EventWriteReceipt, PatchTarget,
    TextEdit,
};
pub use capability::{Capabilities, WriteGuard};
pub use connect_observer::{ConnectObserver, ConnectStep, IgnoreConnectSteps};
pub use connection::{ConnectionInfo, HttpVersion, TlsVersion};
#[cfg(feature = "http")]
pub use connection::{ObservedHttpVersion, same_origin};
pub use contact::{
    ContactDestination, ContactPhoto, ContactSourceSync, ContactUnavailable, ContactWriteReceipt,
    ContactsProvider,
};
pub use error::{ProviderError, ProviderResult};
pub use mail_edit::{MailEdit, MailEditReceipt};
pub use page::{PageToken, SyncKind, SyncPage};
pub use provider::Provider;
pub use shared::{SharedMailbox, SharedMailboxes};
pub use stream::{EmailChunk, EmailStream, PassMode, split_page};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    SubmissionReceipt,
};
pub use sync::ScopeSync;
pub use watch::{Watch, WatchEvent};

#[cfg(test)]
mod tests;
