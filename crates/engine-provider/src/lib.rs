//! `engine-provider` — the provider/transport trait surface.
//!
//! A provider adapter turns a remote account's mail and calendar state into the
//! engine's normalized, provider-neutral shapes. This crate defines the **small**
//! contract every adapter implements so the sync orchestrator and stores never
//! switch on provider kind (`providers.md`):
//!
//! - return a normalized [`SyncUpdate`] plus an opaque next cursor, bundled as [`ScopeSync`] — or,
//!   for a responsive UI, one [`SyncPage`] at a time;
//! - expose what it can do via [`Capabilities`];
//! - classify failures through [`ProviderError`] (the engine-neutral
//!   [`FailureClass`](engine_core::error::FailureClass) taxonomy);
//! - signal delta-vs-snapshot (carried inside the [`SyncUpdate`] itself).
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
mod error;
mod mail_edit;
mod page;
mod submit;
mod sync;
mod watch;

use std::collections::BTreeSet;

use async_trait::async_trait;
pub use calendar_write::{EventDeletion, EventWrite, EventWriteReceipt, WritePrecondition};
pub use capability::Capabilities;
use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    mail::{Mailbox, Message},
    raw::RawMime,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};
pub use error::{ProviderError, ProviderResult};
pub use mail_edit::{MailEdit, MailEditReceipt};
pub use page::{PageToken, SyncKind, SyncPage};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    SubmissionReceipt,
};
pub use sync::ScopeSync;
pub use watch::{Watch, WatchEvent};

/// Default page size [`Provider::sync_email`] uses to drain
/// [`Provider::sync_email_page`]. Streaming callers pass their own, smaller limit
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
    /// The data domains this adapter supports.
    fn capabilities(&self) -> &Capabilities;

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

    /// Fetches **one page** of the account's mail objects since `cursor` — the
    /// paged primitive every adapter implements.
    ///
    /// `page` is the opaque continuation from the previous page's
    /// [`SyncPage::next_page`] (`None` starts the pass); `limit` bounds the page
    /// size, and the adapter may clamp it to a protocol maximum (JMAP
    /// `maxObjectsInGet`) and treats `0` as that maximum. A first pass (`cursor`
    /// `None`, or when the provider can no longer compute a delta —
    /// `cannotCalculateChanges`) is a [`SyncKind::Snapshot`]; each snapshot page
    /// carries the ids it covers in [`SyncPage::present`] so the orchestrator can
    /// tombstone at end of pass. All pages of one pass share
    /// [`SyncPage::kind`]/[`SyncPage::total`]; [`SyncPage::next_cursor`] is only
    /// meaningful on the final page.
    ///
    /// [`Provider::sync_email`] drains this into one update; a responsive caller
    /// drives it directly and applies each page as it lands (`engine-sync`). Mail
    /// providers ([`Capabilities::mail`]) override this; the default rejects.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] classified per
    /// [`FailureClass`](engine_core::error::FailureClass).
    async fn sync_email_page(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        let _ = (account, cursor, page, limit);
        Err(ProviderError::invalid_state(
            "provider does not support mail sync",
        ))
    }

    /// Fetches the account's mail objects since `cursor` as a single combined
    /// update (a full snapshot when `cursor` is `None`, or when the provider can
    /// no longer compute a delta — JMAP `cannotCalculateChanges`).
    ///
    /// This default **drains** [`Provider::sync_email_page`] page by page and
    /// merges the pages into one [`ScopeSync`], so adapters implement only the
    /// paged primitive. Callers that want a responsive, incrementally-applied sync
    /// should drive [`Provider::sync_email_page`] directly (see `engine-sync`'s
    /// streaming loop) rather than this whole-scope convenience.
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
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let mut present = BTreeSet::new();
        let mut page_token: Option<PageToken> = None;
        let kind;
        let next_cursor;
        loop {
            let page = self
                .sync_email_page(account, cursor, page_token.as_ref(), DEFAULT_DRAIN_PAGE)
                .await?;
            changed.extend(page.changed);
            removed.extend(page.removed);
            present.extend(page.present);
            let Some(token) = page.next_page else {
                kind = page.kind;
                next_cursor = page.next_cursor;
                break;
            };
            page_token = Some(token);
        }
        let update = match kind {
            SyncKind::Snapshot => SyncUpdate::snapshot(changed, present),
            SyncKind::Delta => SyncUpdate::delta(changed, removed),
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

    /// Creates or replaces a calendar object resource (CalDAV `PUT`).
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the
    /// default rejects, so a capability-checking caller never relies on it. The
    /// write is outbox-mediated by the caller (a durable pending op precedes this
    /// side effect); this method performs only the provider call. The body is the
    /// round-tripped [`RawIcal`](engine_core::raw::RawIcal), never a re-serialized
    /// projection (`calendar-semantics.md`); optimistic concurrency rides on the
    /// [`WritePrecondition`].
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]. A precondition failure
    /// (`If-Match`/`If-None-Match`) is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict) —
    /// refetch and merge before retrying; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn put_event(
        &self,
        account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        let _ = (account, write);
        Err(ProviderError::invalid_state(
            "provider does not support calendar writes",
        ))
    }

    /// Deletes a calendar object resource (CalDAV `DELETE`), optionally guarded by
    /// an `If-Match` ETag.
    ///
    /// Providers advertising [`Capabilities::calendar_writes`] override this; the
    /// default rejects. Outbox-mediated by the caller, like [`Provider::put_event`].
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; an `If-Match` failure is
    /// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict), and
    /// the default returns
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
