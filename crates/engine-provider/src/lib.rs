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
//! The trait is deliberately **shaped by JMAP** and kept minimal: it covers the
//! mail spine (mailboxes + email), calendar, submission, and contact contracts.
//! It depends only on `engine-core`; network access
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
mod stream;
mod submit;
mod sync;
mod watch;

pub use calendar_write::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, EventWrite, EventWriteReceipt,
    PatchTarget, RsvpResponse, TextEdit, WritePrecondition,
};
pub use capability::{Capabilities, RsvpControls, WriteGuard};
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
pub use stream::{EmailChunk, EmailStream, PassMode, split_page};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    DraftCalendar, SubmissionReceipt,
};
pub use sync::ScopeSync;
pub use watch::{Watch, WatchEvent};

/// Default page size [`Provider::sync_email`] uses to drain
/// [`Provider::stream_email`]. Streaming callers pass their own, smaller limit
/// for a more responsive UI (see `engine-sync`).
pub(crate) const DEFAULT_DRAIN_PAGE: usize = 500;

#[cfg(test)]
mod tests;
