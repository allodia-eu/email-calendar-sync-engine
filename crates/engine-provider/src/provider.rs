//! The [`Provider`] trait: the one seam every adapter implements.
//!
//! Lifted out of the crate root purely for size — the trait is the crate's whole surface,
//! and the root now carries the module docs, the module tree and the re-exports. Nothing
//! about it changed in the move.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, ProviderKey},
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncWindow},
};

// `Capabilities`, `EmailChunk` and `PageToken` are named only by the doc links here, but
// rustdoc resolves those against the *module's* scope — a link that worked in the crate root
// silently breaks on a move, and this crate denies rustdoc warnings, so the move would fail
// the build rather than quietly produce dead links.
#[allow(
    unused_imports,
    reason = "named by intra-doc links on the trait's methods"
)]
use crate::{Capabilities, EmailChunk, PageToken, PassMode, ReportControls, RsvpControls};
use crate::{
    ConnectionInfo, DEFAULT_DRAIN_PAGE, Draft, EmailStream, EventDeletion, EventDraft, EventEdit,
    EventRsvp, EventWrite, EventWriteReceipt, MailEdit, MailEditReceipt, MessageReport,
    ProviderError, ProviderResult, ReportReceipt, ScopeSync, SubmissionReceipt, error::unsupported,
};

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
        Err(unsupported("mail sync"))
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
            Err(unsupported("mail sync"))
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
        crate::stream::drain_email(self.stream_email(
            account,
            cursor,
            self.default_sync_window(),
            DEFAULT_DRAIN_PAGE,
            0,
        ))
        .await
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
        Err(unsupported("mail submission"))
    }

    /// Files the sender's copy of an **already-delivered** message, repairing a submission
    /// that came back [`SentCopy::Unfiled`](crate::SentCopy::Unfiled). Sends nothing.
    ///
    /// Only a transport that files the copy as a separate operation implements this
    /// (IMAP/SMTP); one that files it within the send never reports `Unfiled`, so this
    /// default is unreachable from a correct caller. **Implementations must be idempotent**:
    /// it sits behind a button on a message that has already gone out, so it will be pressed
    /// twice — check whether the copy is there before placing another.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] when the copy could not be filed; the caller may offer
    /// the retry again. The default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn file_sent_copy(
        &self,
        account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<ProviderKey> {
        let _ = (account, draft);
        Err(ProviderError::invalid_state(
            "provider files the sent copy as part of the send",
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
        Err(unsupported("mail writes"))
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
        Err(unsupported("message source fetch"))
    }

    /// Reports `report.target` to the provider as junk, not junk, or phishing.
    ///
    /// A report is not a move: the provider files the message itself, so a caller that
    /// reports must not also move. Providers advertising
    /// [`Capabilities::mail_report`] override this; the default rejects, so a
    /// capability-checking caller never relies on it.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`].
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState)
    /// for a verdict the transport cannot express (via [`ReportControls::accept`]); a
    /// stale target — an IMAP UID under a changed `UIDVALIDITY` — is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict), so the
    /// caller re-syncs and retries.
    async fn report_message(
        &self,
        account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        let _ = (account, report);
        Err(unsupported("reporting a message"))
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
        Err(unsupported("calendar sync"))
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
        Err(unsupported("calendar sync"))
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
        Err(unsupported("calendar writes"))
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
        Err(unsupported("calendar writes"))
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
        Err(unsupported("whole-document calendar writes"))
    }

    /// Answers an invitation: sets **the account's own** participation status, and lets the
    /// server tell the organizer.
    ///
    /// Not an [`EventEdit`] of the attendee array, though it changes the same bytes: every
    /// transport routes scheduling through a distinct verb, so a patch would change the
    /// status and tell nobody. `base` is the event as the caller read it — the document the
    /// surgery runs over on a document transport, and the revision the write is guarded by.
    ///
    /// `rsvp.attendee` is the address the invitation **matched**, which on an aliased
    /// account is not the account's primary identity; an adapter uses it verbatim and never
    /// derives one ([`EventRsvp`]).
    ///
    /// Providers advertising [`Capabilities::calendar_rsvp`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](Provider::create_event).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A guard failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) — refetch and
    /// re-answer, **never** blind-retry. An event with no `ATTENDEE` for that address, or a
    /// request for a control this transport does not honour (a `comment`, or
    /// `notify_organizer: false`, against [`RsvpControls`]), is
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) —
    /// refused rather than silently dropped. The default returns the same.
    async fn rsvp_event(
        &self,
        account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, base, rsvp);
        Err(unsupported("answering invitations"))
    }

    /// Deletes an event, or one occurrence of it, guarded by the revision the caller read.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the default
    /// rejects. Outbox-mediated by the caller, like [`create_event`](Provider::create_event).
    /// An event that is **already gone** is a success, not an error: the delete is
    /// idempotent, so a retry of one that already landed resolves cleanly.
    ///
    /// `base` is the event as the caller read it, when the caller has it. A
    /// [`Series`](crate::DeleteTarget::Series) delete needs nothing from it — the stored object
    /// goes whole — which is why it is optional. Removing one **occurrence** is a rewrite of
    /// the series on a document transport, so CalDAV needs the stored bytes and says so
    /// rather than guessing; the other three derive what they need from the deletion itself.
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
        base: Option<&Event>,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        let _ = (account, base, deletion);
        Err(unsupported("calendar writes"))
    }
}
