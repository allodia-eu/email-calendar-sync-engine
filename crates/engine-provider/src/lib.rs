//! `engine-provider` — the provider/transport trait surface.
//!
//! A provider adapter turns a remote account's mail and calendar state into the
//! engine's normalized, provider-neutral shapes. This crate defines the **small**
//! contract every adapter implements so the sync orchestrator and stores never
//! switch on provider kind (`providers.md`):
//!
//! - return a normalized [`SyncUpdate`] plus an opaque next cursor, bundled as [`ScopeSync`] — or,
//!   for a responsive UI, one [`SyncPage`] at a time;
//! - expose what it can do, and what its transport negotiated, via one [`ConnectionInfo`];
//! - classify failures through [`ProviderError`] (the engine-neutral
//!   [`FailureClass`](engine_core::error::FailureClass) taxonomy);
//! - signal delta-vs-snapshot (carried inside the [`SyncUpdate`] itself).
//!
//! [`ConnectionInfo`] reports the *outcome* of a connect. The phase itself — the
//! well-known redirects, the TLS handshake, authentication, endpoint discovery — is
//! observable through [`ConnectStep`] and [`ConnectObserver`], which an adapter's
//! config carries.
//!
//! The trait is deliberately **shaped by JMAP** and kept minimal: it covers the
//! step-4 mail spine (mailboxes + email) and grows a method at a time as slices
//! land (submission, calendar). It depends only on `engine-core`; network access
//! and an async runtime live in the concrete provider crates (`provider-jmap`,
//! and later `provider-imap`/`provider-smtp`/`provider-caldav`). The full
//! orchestrator that drives many providers and scopes is a later build step; the
//! step-4 driver is the thin loop in `engine-sync`.

mod boxed;
mod calendar_write;
mod capability;
mod connect_observer;
mod connection;
mod error;
mod mail_edit;
mod page;
mod stream;
mod submit;
mod sync;
mod watch;

use std::collections::BTreeSet;

use async_trait::async_trait;
pub use calendar_write::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventWrite, EventWriteReceipt, PatchTarget,
    TextEdit,
};
pub use capability::{Capabilities, WriteGuard};
pub use connect_observer::{ConnectObserver, ConnectStep, IgnoreConnectSteps};
#[cfg(feature = "http")]
pub use connection::ObservedHttpVersion;
pub use connection::{ConnectionInfo, HttpVersion, TlsVersion};
use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
};
pub use error::{ProviderError, ProviderResult};
pub use mail_edit::{MailEdit, MailEditReceipt};
pub use page::{PageToken, SyncKind, SyncPage};
pub use stream::{EmailChunk, EmailStream, PassMode, split_page};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    SubmissionReceipt,
};
pub use sync::ScopeSync;
pub use watch::{Watch, WatchEvent};

/// Default page size [`Provider::sync_email`] uses to drain
/// [`Provider::stream_email`]. Streaming callers pass their own, smaller limit
/// for a more responsive UI (see `engine-sync`).
const DEFAULT_DRAIN_PAGE: usize = 500;

/// A read/sync provider adapter for one account's mail (and, as slices land,
/// calendar and submission).
///
/// Each `sync_*` method fetches the changes for one scope since `cursor` (or a
/// first full snapshot when `cursor` is `None`) and returns them as a
/// [`ScopeSync`]. The matching `*_scope` accessor names the [`SyncScope`] the
/// orchestrator claims and applies under, so callers do not hard-code a provider's
/// scope granularity. Adapters own protocol pagination, batching, retries, and
/// quirks; the store owns atomic application.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Everything this adapter learned about its connection once it was established:
    /// the data domains it can serve ([`ConnectionInfo::capabilities`]) and the
    /// transport versions the server negotiated.
    ///
    /// The one post-connect seam — callers read facts from it and never switch on
    /// provider kind (`providers.md`). The returned value is a cheap `Copy`, so an
    /// adapter may either store it or compose it per call.
    fn connection_info(&self) -> ConnectionInfo;

    /// The scope the account's mail collections (mailboxes/folders/labels) sync
    /// under. Defaults to the JMAP `(account, Mailbox)` scope; mail providers with
    /// a different granularity (IMAP) override it. A calendar-only provider never
    /// has this consulted (its [`Capabilities::mail`] is false).
    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    /// The scope the account's mail objects sync under. Defaults to the JMAP
    /// `(account, Email)` scope; non-JMAP mail providers override.
    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    /// Fetches the account's mail collections since `cursor` (a full snapshot when
    /// `cursor` is `None`).
    ///
    /// Containers are applied before the members that reference them
    /// (`store-and-sync.md` referential apply order), so the orchestrator syncs
    /// this scope before [`Provider::sync_email`]. Mail providers
    /// ([`Capabilities::mail`]) override this; the default rejects, so a
    /// capability-checking caller never relies on it.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] classified per
    /// [`FailureClass`](engine_core::error::FailureClass): transport/auth/rate-limit/conflict/
    /// invalid-state/needs-resync/permanent.
    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let _ = (account, cursor);
        Err(ProviderError::invalid_state(
            "provider does not support mail sync",
        ))
    }

    /// The default sync window the **whole-scope** [`Provider::sync_email`]
    /// convenience fetches under, when a caller does not stream with an explicit
    /// one. Defaults to the full history; a provider whose depth is configured at
    /// construction (IMAP `with_since`) overrides it. The streaming path takes its
    /// window explicitly (see [`Provider::stream_email`]), so a host changes depth
    /// per sync without reconnecting.
    fn default_sync_window(&self) -> SyncWindow {
        SyncWindow::full()
    }

    /// Streams one email sync pass since `cursor`, bounded by `window`, as
    /// incremental [`EmailChunk`]s — the paged primitive every mail adapter
    /// implements.
    ///
    /// The two knobs it separates (`store-and-sync.md`):
    /// - `fetch_batch` bounds each **network round trip** (an IMAP `UID FETCH` window, a JMAP
    ///   `Email/get` page, a Graph `$top`); `0` means the adapter's protocol maximum.
    /// - `chunk_size` bounds how many messages accumulate before a chunk is **yielded** — the
    ///   streaming granularity a host commits and renders; `0` means one chunk per batch.
    ///
    /// A large `fetch_batch` with a small `chunk_size` gives *both* few round trips
    /// *and* row-as-it-arrives commits. The returned [`EmailStream`] borrows `self`
    /// and the arguments; the adapter's fetch advances only as the stream is polled
    /// (backpressure). Each chunk carries a [`PassMode`] and an optional
    /// [`advance_to`](EmailChunk::advance_to) checkpoint telling the orchestrator
    /// how to apply and how far to advance the cursor, so a killed cold sync resumes
    /// (`store-and-sync.md`).
    ///
    /// Mail providers ([`Capabilities::mail`]) override this; the default yields a
    /// single classified `Err`, so a capability-checking caller never relies on it.
    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        let _ = (account, cursor, window, fetch_batch, chunk_size);
        Box::pin(futures_util::stream::once(async {
            Err(ProviderError::invalid_state(
                "provider does not support mail sync",
            ))
        }))
    }

    /// Fetches the account's mail objects since `cursor` as a single combined
    /// update (a full snapshot when `cursor` is `None`, or when the provider can
    /// no longer compute a delta — JMAP `cannotCalculateChanges`).
    ///
    /// This default **drains** [`Provider::stream_email`] into one [`ScopeSync`], so
    /// adapters implement only the streaming primitive. Callers that want a
    /// responsive, incrementally-applied sync drive [`Provider::stream_email`]
    /// directly (see `engine-sync`'s streaming loop) rather than this whole-scope
    /// convenience. It fetches under [`Provider::default_sync_window`].
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] classified per
    /// [`FailureClass`](engine_core::error::FailureClass).
    async fn sync_email(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Message>> {
        use futures_util::StreamExt;

        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let mut present = BTreeSet::new();
        let mut mode = PassMode::Additive;
        let mut next_cursor: Option<SyncState> = None;
        let mut stream = self.stream_email(
            account,
            cursor,
            self.default_sync_window(),
            DEFAULT_DRAIN_PAGE,
            0,
        );
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            mode = chunk.mode;
            changed.extend(chunk.changed);
            removed.extend(chunk.removed);
            present.extend(chunk.present);
            if let Some(cursor) = chunk.advance_to {
                next_cursor = Some(cursor);
            }
        }
        let next_cursor = next_cursor.ok_or_else(|| {
            ProviderError::invalid_state("email stream ended without a final cursor")
        })?;
        // A reconcile pass tombstones against the accumulated present set; an
        // additive pass (cold backfill or delta) carries only explicit removals.
        // For a first sync both are equivalent (nothing local to tombstone).
        let update = match mode {
            PassMode::Reconcile => SyncUpdate::snapshot(changed, present),
            PassMode::Additive => SyncUpdate::delta(changed, removed),
        };
        Ok(ScopeSync::new(update, next_cursor))
    }

    /// Sends `draft`: creates the message and submits it, filing the sent copy.
    ///
    /// Providers advertising [`Capabilities::submission`] override this; the
    /// default rejects, so a caller that checked capabilities first never relies
    /// on it. Submission is outbox-mediated by the caller (a durable pending op
    /// precedes this side effect); this method performs only the provider call.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. The default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn submit_email(
        &self,
        account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        let _ = (account, draft);
        Err(ProviderError::invalid_state(
            "provider does not support mail submission",
        ))
    }

    /// Applies a [`MailEdit`] to an already-synced message: mark-read/flag (keyword
    /// change), move (folder change, incl. a Trash "delete"), or permanent delete.
    ///
    /// Providers advertising [`Capabilities::mail_writes`] override this; the default
    /// rejects, so a capability-checking caller never relies on it. The write is
    /// outbox-mediated by the caller (a durable pending op precedes this side
    /// effect); this method performs only the provider call.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A stale target — e.g. an IMAP UID
    /// whose mailbox `UIDVALIDITY` has since changed — is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict)
    /// (re-sync, then retry); the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn edit_mail(
        &self,
        account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        let _ = (account, edit);
        Err(ProviderError::invalid_state(
            "provider does not support mail writes",
        ))
    }

    /// Fetches the raw RFC 5322 source of an already-synced `message` — the lossless
    /// Tier-3 blob a host fetches on demand to read the body and (later) attachments
    /// (`north-star.md`). Returns the whole message (headers + every part); the
    /// engine extracts displayable text with `engine-mime` and caches the raw in the
    /// store's content-addressed blob area, so one fetch serves the body now and
    /// HTML/attachments later without re-fetching.
    ///
    /// Providers advertising [`Capabilities::message_source`] override this; the
    /// default rejects, so a capability-checking caller never relies on it.
    /// `message` carries everything an adapter needs to address the fetch: its
    /// [`id`](engine_core::mail::Message::id) key (the IMAP `(mailbox, UIDVALIDITY,
    /// UID)`) and its [`blob_id`](engine_core::mail::Message::blob_id) (a JMAP/Graph
    /// download handle).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A stale target — e.g. an IMAP UID
    /// whose mailbox `UIDVALIDITY` has since changed — is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict)
    /// (re-sync, then retry); the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn fetch_message_source(
        &self,
        account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        let _ = (account, message);
        Err(ProviderError::invalid_state(
            "provider does not support message source fetch",
        ))
    }

    /// The scope the account's calendars sync under. Defaults to the JMAP
    /// `(account, Calendar)` scope; non-JMAP providers override.
    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Calendar,
        }
    }

    /// The scope the account's calendar events sync under. Defaults to the JMAP
    /// `(account, CalendarEvent)` scope; non-JMAP providers override.
    fn event_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::CalendarEvent,
        }
    }

    /// Fetches the account's calendar collections since `cursor`. Providers
    /// advertising [`Capabilities::calendars`] override this.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn sync_calendars(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        let _ = (account, cursor);
        Err(ProviderError::invalid_state(
            "provider does not support calendar sync",
        ))
    }

    /// Fetches the account's calendar events since `cursor` (JSCalendar). Providers
    /// advertising [`Capabilities::calendars`] override this.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn sync_events(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let _ = (account, cursor);
        Err(ProviderError::invalid_state(
            "provider does not support calendar sync",
        ))
    }

    /// Creates a new event from an [`EventDraft`].
    ///
    /// The adapter serializes the draft in its own protocol — a document a CalDAV server
    /// stores, a JSCalendar object a JMAP server assigns an id to. The receipt names the
    /// [`EventId`](engine_core::ids::EventId) the create **resolved to**, which is the only
    /// place a server-assigning transport reveals it.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects, so a capability-checking caller never relies on it. Outbox-mediated by the
    /// caller (a durable pending op precedes this side effect); this method performs only
    /// the provider call.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. An event already existing at the target is a
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict); the default
    /// returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn create_event(
        &self,
        account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, draft);
        Err(ProviderError::invalid_state(
            "provider does not support calendar writes",
        ))
    }

    /// Applies an [`EventEdit`] to an already-stored event.
    ///
    /// `base` is the event **as the caller read it**, and it is load-bearing twice over: it
    /// carries the provider-native payload the patch is applied to (so an update never
    /// re-serializes the lossy projection — `calendar-semantics.md`), and the revision the
    /// write is guarded by, so a stale edit is refused rather than clobbering a newer one.
    /// Where the surgery happens differs by transport and is the adapter's business: CalDAV
    /// rewrites the stored `RawIcal` itself and `PUT`s it back, while JMAP hands the patch
    /// to a server whose update verb is already a patch.
    ///
    /// Whether the guard is actually enforced is **not** universal — see
    /// [`Capabilities::calendar_write_guard`].
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](Provider::create_event).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure — the server copy moved on —
    /// is [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict): refetch,
    /// re-apply the edit to the fresh base, resubmit; **never** blind-retry. A patch that
    /// would change the event's time *form* (silently converting a zoned event to a UTC
    /// instant, or an all-day event to a timed one) is rejected, not converted. The default
    /// returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn patch_event(
        &self,
        account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, base, edit);
        Err(ProviderError::invalid_state(
            "provider does not support calendar writes",
        ))
    }

    /// Replaces an event's whole stored document (CalDAV `PUT`).
    ///
    /// **Not** the neutral edit verb — [`patch_event`](Provider::patch_event) is. Only a
    /// document-oriented transport has this, and only an operation naturally expressed as a
    /// finished document should use it (today: the iMIP RSVP primitive). An adapter whose
    /// update verb is already a patch leaves this at the rejecting default *even though it
    /// advertises [`Capabilities::calendar_writes`]* — the capability covers the neutral
    /// spine, not this.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict); an adapter
    /// with no document verb returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState), as
    /// does the default.
    async fn put_event(
        &self,
        account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, write);
        Err(ProviderError::invalid_state(
            "provider does not support whole-document calendar writes",
        ))
    }

    /// Deletes an event, guarded by the revision the caller read.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](Provider::create_event).
    /// An event that is **already gone** is a success, not an error: the delete is
    /// idempotent, so a retry of one that already landed resolves cleanly.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; a guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict), and the
    /// default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn delete_event(
        &self,
        account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        let _ = (account, deletion);
        Err(ProviderError::invalid_state(
            "provider does not support calendar writes",
        ))
    }
}

#[cfg(test)]
mod tests;
