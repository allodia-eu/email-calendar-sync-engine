//! The [`Provider`] trait — the one contract every adapter implements.
//!
//! Split out of `lib.rs` (which keeps the crate documentation and re-exports) so
//! the trait has room to grow inside the 500-line limit.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncWindow},
};

// Named only by the intra-doc links below. Rustdoc resolves those in this module's
// scope, but rustc does not count a doc link as a use — hence the allow rather than a
// `crate::`-qualified path at every mention.
#[allow(unused_imports, reason = "resolves this file's intra-doc links")]
use crate::Capabilities;
use crate::{
    ConnectionInfo, Draft, EmailStream, EventDeletion, EventDraft, EventEdit, EventWrite,
    EventWriteReceipt, MailEdit, MailEditReceipt, ProviderError, ProviderResult, ScopeSync,
    SharedMailbox, SubmissionReceipt, stream::drain_email_stream,
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
    /// incremental [`EmailChunk`](crate::EmailChunk)s — the paged primitive every mail adapter
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
    /// (backpressure). Each chunk carries a [`PassMode`](crate::PassMode) and an optional
    /// [`advance_to`](crate::EmailChunk::advance_to) checkpoint telling the orchestrator
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
        drain_email_stream(self, account, cursor).await
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

    /// Every mail store this **credential** can open, its own included (`shared.rs`).
    ///
    /// There is no `account` parameter, deliberately: like
    /// [`connection_info`](Provider::connection_info) this is a fact about the credential
    /// rather than about any engine account, and it builds no scope. A host calls it
    /// *before* deciding what to onboard, and binds a provider to a chosen store with that
    /// entry's [`handle`](SharedMailbox::handle) — after which the store is just another
    /// account and every existing sync path applies unchanged.
    ///
    /// Adapters advertising
    /// [`SharedMailboxes::Enumerable`](crate::SharedMailboxes::Enumerable) override this;
    /// the default rejects, so a capability-checking caller never relies on it. An adapter
    /// whose server has no list API leaves it at the default and offers
    /// [`resolve_shared_mailbox`](Provider::resolve_shared_mailbox) instead.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn list_shared_mailboxes(&self) -> ProviderResult<Vec<SharedMailbox>> {
        Err(ProviderError::invalid_state(
            "provider cannot enumerate shared mailboxes",
        ))
    }

    /// Resolves one mail store by `address`, proving the credential can actually open it.
    ///
    /// The verb for a server that will not enumerate
    /// ([`SharedMailboxes::ByAddress`](crate::SharedMailboxes::ByAddress)), and equally
    /// available on an enumerable one — verifying an address the user typed is useful
    /// either way. `address` is user input that an adapter may have to splice into a
    /// request path, so validating and encoding it is the adapter's responsibility.
    ///
    /// The resolved [`SharedMailbox::handle`], not `address`, is what a host stores: an
    /// address is not canonical (a Microsoft 365 alias resolves to its target mailbox, so
    /// two addresses can name one store).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. Two negatives are distinguished, by the
    /// remedy each implies rather than by the wording of the server's complaint:
    ///
    /// - **there is no mailbox here for this credential** →
    ///   [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent): nothing about
    ///   the credential would make it resolve.
    /// - **the credential's grant does not cover the request** →
    ///   [`FailureClass::Authentication`](engine_core::error::FailureClass::Authentication): a
    ///   re-consent, a broader scope, or an administrator's grant is what would make it succeed.
    ///
    /// Whether a provider can even *tell those apart* is not universal, and a caller must
    /// not assume it: Microsoft Graph answers a mailbox that exists but has not been shared
    /// with the caller as a plain not-found, refusing to disclose that it is there
    /// (`graph.md`). So `Permanent` means "not resolvable by you", not "does not exist".
    ///
    /// The default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn resolve_shared_mailbox(&self, address: &str) -> ProviderResult<SharedMailbox> {
        let _ = address;
        Err(ProviderError::invalid_state(
            "provider cannot resolve shared mailboxes",
        ))
    }
}

#[cfg(test)]
#[path = "provider_shared_tests.rs"]
mod shared_tests;
