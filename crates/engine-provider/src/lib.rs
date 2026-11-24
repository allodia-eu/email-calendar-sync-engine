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
//! - bound every wait on a server by the one shared [`Deadlines`];
//! - signal delta-vs-snapshot (carried inside the [`SyncUpdate`](engine_core::sync::SyncUpdate)
//!   itself).
//!
//! [`ConnectionInfo`] reports the *outcome* of a connect. The phase itself — the
//! well-known redirects, the TLS handshake, authentication, endpoint discovery — is
//! observable through [`ConnectStep`] and [`ConnectObserver`], which an adapter's
//! config carries.
//!
//! Credentials are asked for, not held (`credential`): an adapter asks the host's
//! [`CredentialSource`] each time it presents one, tells it the protocol and [`Origin`], waits
//! for it no longer than the step it belongs to, and asks once for a replacement when the server
//! refuses the answer ([`with_credential`]).
//!
//! A credential often reaches more than its own mailbox and calendar. [`SharedAccounts`] and
//! [`SharedAccount`] are the discovery seam for that (`shared`): they answer *which stores
//! can I open*, and stop there — a discovered store becomes an ordinary account bound with
//! the same credential, so no scope, cursor or account kind is added.
//!
//! The trait is deliberately **shaped by JMAP** and kept minimal: it covers the
//! mail spine (mailboxes + email), calendar, submission, and contact contracts.
//! It depends on `engine-core`, and on tokio's timer alone (no runtime) to bound an ask to a
//! credential source; network access and the runtime live in the concrete provider crates and
//! the host (`provider-jmap`,
//! plus `provider-imap`/`provider-smtp`/`provider-caldav`). `engine-sync` drives
//! the provider-neutral scopes.

mod boxed;
mod calendar_sync;
mod calendar_user;
mod calendar_write;
mod capability;
mod capability_calendar;
mod capability_contacts;
mod capability_mailbox;
mod capability_submission;
mod capability_window;
mod collection;
mod connect_observer;
mod connection;
mod contact;
mod content_id;
mod credential;
mod deadline;
mod draft_receipt;
mod error;
mod hand_over;
mod identity;
#[cfg(feature = "import")]
pub mod import;
mod keyword_name;
mod mail_edit;
mod mail_rules;
mod mailbox_edit;
mod mailbox_write;
mod meeting;
mod page;
mod provider;
#[cfg(feature = "http")]
mod redirect;
mod report;
mod rule_listing;
mod shared;
mod sources;
mod stream;
mod stream_update;
mod submit;
mod sync;
mod vacation;
mod watch;
mod write_guard;

pub use calendar_sync::CalendarSync;
pub use calendar_user::CalendarUserAddresses;
pub use calendar_write::{
    AttachedConference, CalendarAddress, CalendarWrites, ConferenceEdit, DeleteTarget,
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, EventWrite,
    EventWriteReceipt, InvitationError, Invitee, InviteePatch, InviteeRole, MeetingDraft,
    MeetingText, NewConference, Occurrence, PatchTarget, RecurrenceEdit, ReplyDelivery,
    RsvpResponse, SchedulingIdentity, TextEdit, WritePrecondition, meeting_text,
    refuse_native_conference,
};
pub use capability::Capabilities;
pub use capability_calendar::{OverrideSurvival, RsvpControls};
pub use collection::{AddressBookCollections, CalendarCollections, NewAddressBook, NewCalendar};
pub use connect_observer::{ConnectObserver, ConnectService, ConnectStep, IgnoreConnectSteps};
pub use connection::{ConnectionInfo, HttpVersion, ObservedTlsVersion, TlsVersion};
#[cfg(feature = "http")]
pub use connection::{ObservedHttpVersion, same_origin};
pub use contact::{
    ContactDestination, ContactPhoto, ContactSourceSync, ContactUnavailable, ContactWriteReceipt,
    ContactsProvider,
};
pub use credential::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource,
    CredentialSubject, FixedCredential, Origin, Secret, with_credential,
};
pub use deadline::Deadlines;
pub use draft_receipt::DraftReceipt;
pub use error::{ProviderError, ProviderResult, TargetRefusal};
pub use hand_over::{HandOver, HandOverLog, HandedOver, Unrecorded};
pub use identity::{IdentityControls, SenderIdentity, SenderIdentityId};
pub use keyword_name::{KeywordName, keyword_named};
pub use mail_edit::{MailEdit, MailEditReceipt};
pub use mail_rules::{MailRules, guard_version};
pub use mailbox_edit::{MailboxEdit, MailboxEditReceipt};
pub use mailbox_write::MailboxWrites;
pub use meeting::{
    JitsiMeetings, JitsiServerError, MeetingKey, MeetingKeyError, MeetingRequest, MeetingService,
};
pub use page::{PageToken, SyncKind, SyncPage};
pub use provider::Provider;
#[cfg(feature = "http")]
pub use redirect::{redirect_refused, redirect_target};
pub use report::{
    MessageReport, ReportControls, ReportEvidence, ReportReceipt, ReportVerdict, ReportVerdicts,
};
pub use rule_listing::{RuleListing, digest_hex};
pub use shared::{SharedAccount, SharedAccounts, StoreDomains};
pub use sources::{SourceStream, one_at_a_time};
pub use stream::{EmailChunk, EmailStream, PassMode, split_page};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    DraftCalendar, SentCopy, SubmissionReceipt,
};
pub use sync::{CalendarListing, ScopeSync};
pub use vacation::{Vacation, VacationPrecondition, refuse_unheld_vacation};
pub use watch::{Watch, WatchEvent};
pub use write_guard::WriteGuard;

/// Default page size [`Provider::sync_email`] uses to drain
/// [`Provider::stream_email`]. Streaming callers pass their own, smaller limit
/// for a more responsive UI (see `engine-sync`).
pub(crate) const DEFAULT_DRAIN_PAGE: usize = 500;

#[cfg(test)]
mod calendar_default_tests;
#[cfg(test)]
#[path = "draft_tests.rs"]
mod draft_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod widen_tests;
