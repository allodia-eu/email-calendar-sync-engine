//! `engine-provider` — the provider/transport trait surface.
//!
//! A provider adapter turns a remote account's mail and calendar state into the
//! engine's normalized, provider-neutral shapes. This crate defines the **small**
//! contract every adapter implements so the sync orchestrator and stores never
//! switch on provider kind (`providers.md`):
//!
//! - return a normalized [`SyncUpdate`] plus an opaque next cursor, bundled as
//!   [`ScopeSync`] — or, for a responsive UI, one [`SyncPage`] at a time;
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

mod calendar_write;
mod capability;
mod error;
mod mail_edit;
mod page;
mod submit;
mod sync;

pub use calendar_write::{EventDeletion, EventWrite, EventWriteReceipt, WritePrecondition};
pub use capability::Capabilities;
pub use error::{ProviderError, ProviderResult};
pub use mail_edit::{MailEdit, MailEditReceipt};
pub use page::{PageToken, SyncKind, SyncPage};
pub use submit::{
    ContentIdError, ContentIdHeader, Draft, DraftAttachment, DraftAttachmentDisposition,
    SubmissionReceipt,
};
pub use sync::ScopeSync;

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::calendar::{Calendar, Event};
use engine_core::ids::AccountId;
use engine_core::mail::{Mailbox, Message};
use engine_core::raw::RawMime;
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};

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
    /// Returns a [`ProviderError`] classified per [`FailureClass`](engine_core::error::FailureClass):
    /// transport/auth/rate-limit/conflict/invalid-state/needs-resync/permanent.
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
    /// Returns a [`ProviderError`] classified per [`FailureClass`](engine_core::error::FailureClass).
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
    /// Returns a [`ProviderError`] classified per [`FailureClass`](engine_core::error::FailureClass).
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

/// A boxed provider is itself a [`Provider`], delegating every method to the box's
/// contents — including a `Box<dyn Provider>`, so a host can hold an adapter behind
/// dynamic dispatch.
///
/// The `engine-sync`/`engine-api` functions are generic over `P: Provider`, so a host
/// that picks a concrete adapter at runtime — e.g. a language binding choosing IMAP vs
/// JMAP from account config — needs this to drive them through a trait object. The
/// `?Sized` bound covers the trait-object case for *any* lifetime: a plain
/// `impl Provider for Box<dyn Provider>` is fixed to `'static` and is "not general
/// enough" once the boxed provider is driven from an async task. Kept here, not
/// special-cased in `engine-api` (`engine-api.md`). Every method delegates, so an inner
/// adapter's overrides (submission, calendar writes, a custom drain, …) are honored,
/// not the trait defaults.
#[async_trait]
impl<P: Provider + ?Sized> Provider for Box<P> {
    fn capabilities(&self) -> &Capabilities {
        (**self).capabilities()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        (**self).mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        (**self).email_scope(account)
    }

    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        (**self).sync_mailboxes(account, cursor).await
    }

    async fn sync_email_page(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        (**self).sync_email_page(account, cursor, page, limit).await
    }

    async fn sync_email(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Message>> {
        (**self).sync_email(account, cursor).await
    }

    async fn submit_email(
        &self,
        account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        (**self).submit_email(account, draft).await
    }

    async fn edit_mail(
        &self,
        account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        (**self).edit_mail(account, edit).await
    }

    async fn fetch_message_source(
        &self,
        account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        (**self).fetch_message_source(account, message).await
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        (**self).calendar_scope(account)
    }

    fn event_scope(&self, account: &AccountId) -> SyncScope {
        (**self).event_scope(account)
    }

    async fn sync_calendars(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        (**self).sync_calendars(account, cursor).await
    }

    async fn sync_events(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        (**self).sync_events(account, cursor).await
    }

    async fn put_event(
        &self,
        account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        (**self).put_event(account, write).await
    }

    async fn delete_event(
        &self,
        account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        (**self).delete_event(account, deletion).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::ids::{MailboxId, MessageId};
    use engine_core::membership::Memberships;
    use engine_core::sync::{JmapDataType, SyncUpdate};

    /// A trivial in-memory provider, proving the trait is implementable and that
    /// the scope accessors + capabilities + ScopeSync compose as intended.
    struct FakeJmap {
        caps: Capabilities,
    }

    #[async_trait]
    impl Provider for FakeJmap {
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }

        fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
            SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Mailbox,
            }
        }

        fn email_scope(&self, account: &AccountId) -> SyncScope {
            SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Email,
            }
        }

        async fn sync_mailboxes(
            &self,
            _account: &AccountId,
            _cursor: Option<&SyncState>,
        ) -> ProviderResult<ScopeSync<Mailbox>> {
            let mailbox = Mailbox::new(MailboxId::try_from("a").unwrap(), "Inbox");
            Ok(ScopeSync::new(
                SyncUpdate::delta(vec![mailbox], vec![]),
                SyncState::new("mbox-1"),
            ))
        }

        async fn sync_email_page(
            &self,
            _account: &AccountId,
            cursor: Option<&SyncState>,
            page: Option<&PageToken>,
            _limit: usize,
        ) -> ProviderResult<SyncPage<Message>> {
            // One email in a single page: the continuation is always `None`.
            assert!(page.is_none(), "fake yields a single page");
            let msg = Message::new(
                MessageId::try_from("eaaaaab").unwrap(),
                Memberships::of_one(MailboxId::try_from("a").unwrap()),
            );
            let key = msg.id.key().clone();
            // A first sync (no cursor) is a snapshot; a later one is a delta.
            let (kind, present) = if cursor.is_none() {
                (SyncKind::Snapshot, vec![key])
            } else {
                (SyncKind::Delta, vec![])
            };
            Ok(SyncPage {
                kind,
                changed: vec![msg],
                removed: vec![],
                present,
                next_page: None,
                next_cursor: SyncState::new("email-2"),
                total: Some(1),
            })
        }
    }

    fn account() -> AccountId {
        AccountId::try_from("acct-1").unwrap()
    }

    #[tokio::test]
    async fn provider_returns_scoped_updates_and_cursors() {
        let provider = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        assert!(provider.capabilities().mail());
        assert_eq!(
            provider.email_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::Email,
            }
        );
        assert_eq!(
            provider.mailbox_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::Mailbox,
            }
        );

        // First email sync (no cursor) is a snapshot; mailboxes a delta.
        let mboxes = provider.sync_mailboxes(&account(), None).await.unwrap();
        assert!(!mboxes.is_snapshot());
        assert_eq!(mboxes.next_cursor.as_str(), "mbox-1");

        let first = provider.sync_email(&account(), None).await.unwrap();
        assert!(first.is_snapshot());
        let next = first.next_cursor.clone();
        let second = provider.sync_email(&account(), Some(&next)).await.unwrap();
        assert!(!second.is_snapshot());
    }

    #[tokio::test]
    async fn email_page_primitive_drives_the_drain_default() {
        let provider = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };

        // The paged primitive: a first pass (no cursor) is a one-page snapshot
        // that carries the ids it covers and its own progress total.
        let page = provider
            .sync_email_page(&account(), None, None, 50)
            .await
            .unwrap();
        assert_eq!(page.kind, SyncKind::Snapshot);
        assert_eq!(page.total, Some(1));
        assert!(page.next_page.is_none());
        assert_eq!(page.present.len(), 1);
        assert_eq!(page.next_cursor.as_str(), "email-2");

        // The default drain merges the page(s) back into one snapshot update,
        // advancing to the final page's cursor — adapters implement only paging.
        let drained = provider.sync_email(&account(), None).await.unwrap();
        assert!(drained.is_snapshot());
        assert_eq!(drained.next_cursor.as_str(), "email-2");
    }

    #[tokio::test]
    async fn submit_email_defaults_to_unsupported() {
        use engine_core::error::FailureClass;
        use engine_core::ids::MessageIdHeader;
        use engine_core::mail::EmailAddress;

        let provider = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        // A mail-only provider that did not override submission rejects the call,
        // so a capability-checking caller never depends on the default.
        let draft = crate::Draft::new(
            MessageIdHeader::new("gen-1@host").unwrap(),
            EmailAddress::new("a@host"),
            vec![EmailAddress::new("b@host")],
            "Hi",
            "body",
        );
        let err = provider.submit_email(&account(), &draft).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::InvalidState);
    }

    /// A provider implementing only the required `capabilities`, leaving every other
    /// method to its trait default — so boxing it exercises the blanket impl's
    /// delegation to the *defaults*, not just to an adapter's overrides.
    struct BareProvider {
        caps: Capabilities,
    }

    impl Provider for BareProvider {
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
    }

    #[tokio::test]
    async fn box_dyn_provider_delegates_overrides_and_defaults() {
        use engine_core::error::FailureClass;
        use engine_core::ids::{EventId, MessageIdHeader, Uid};
        use engine_core::mail::EmailAddress;
        use engine_core::raw::RawIcal;

        let email_scope = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        };
        let mailbox_scope = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Mailbox,
        };

        // (1) An adapter that overrides the mail methods: the box yields the inner's
        // data (delegation honors overrides), and the working paged primitive drives
        // the inherited drain default.
        let over: Box<dyn Provider> = Box::new(FakeJmap {
            caps: Capabilities::none().with_mail(),
        });
        assert!(over.capabilities().mail());
        assert_eq!(over.email_scope(&account()), email_scope);
        assert_eq!(over.mailbox_scope(&account()), mailbox_scope);
        assert!(over.sync_mailboxes(&account(), None).await.is_ok());
        let page = over
            .sync_email_page(&account(), None, None, 50)
            .await
            .unwrap();
        assert_eq!(page.kind, SyncKind::Snapshot);
        assert!(
            over.sync_email(&account(), None)
                .await
                .unwrap()
                .is_snapshot()
        );

        // (2) A bare adapter: the box delegates to the trait defaults for every
        // non-required method — the scope defaults compute, the unsupported async
        // operations reject with `InvalidState`.
        let bare: Box<dyn Provider> = Box::new(BareProvider {
            caps: Capabilities::none(),
        });
        assert!(!bare.capabilities().mail());
        assert_eq!(bare.mailbox_scope(&account()), mailbox_scope);
        assert_eq!(bare.email_scope(&account()), email_scope);
        assert_eq!(
            bare.calendar_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::Calendar,
            }
        );
        assert_eq!(
            bare.event_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::CalendarEvent,
            }
        );
        let rejected = [
            bare.sync_mailboxes(&account(), None).await.unwrap_err(),
            bare.sync_email_page(&account(), None, None, 0)
                .await
                .unwrap_err(),
            bare.sync_email(&account(), None).await.unwrap_err(),
            bare.sync_calendars(&account(), None).await.unwrap_err(),
            bare.sync_events(&account(), None).await.unwrap_err(),
        ];
        for err in &rejected {
            assert_eq!(err.class(), FailureClass::InvalidState);
        }

        let draft = crate::Draft::new(
            MessageIdHeader::new("g@host").unwrap(),
            EmailAddress::new("a@host"),
            vec![EmailAddress::new("b@host")],
            "Hi",
            "body",
        );
        assert_eq!(
            bare.submit_email(&account(), &draft)
                .await
                .unwrap_err()
                .class(),
            FailureClass::InvalidState
        );
        let href = EventId::try_from("/cal/e.ics").unwrap();
        let write = crate::EventWrite::create(
            href.clone(),
            Uid::new("e@host").unwrap(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        );
        assert_eq!(
            bare.put_event(&account(), &write)
                .await
                .unwrap_err()
                .class(),
            FailureClass::InvalidState
        );
        let deletion = crate::EventDeletion::unconditional(href);
        assert_eq!(
            bare.delete_event(&account(), &deletion)
                .await
                .unwrap_err()
                .class(),
            FailureClass::InvalidState
        );
    }

    #[tokio::test]
    async fn mail_writes_default_to_unsupported() {
        use engine_core::error::FailureClass;
        use engine_core::ids::ProviderKey;

        let edit = crate::MailEdit::delete(ProviderKey::new("imap:v1:u7@INBOX").unwrap());
        // A mail adapter that did not override writes rejects, so a
        // capability-checking caller never depends on the default — and a boxed
        // adapter delegates `edit_mail` to that same default (the blanket impl).
        let direct = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        let boxed: Box<dyn Provider> = Box::new(FakeJmap {
            caps: Capabilities::none().with_mail(),
        });
        for err in [
            direct.edit_mail(&account(), &edit).await.unwrap_err(),
            boxed.edit_mail(&account(), &edit).await.unwrap_err(),
        ] {
            assert_eq!(err.class(), FailureClass::InvalidState);
        }
    }

    #[tokio::test]
    async fn message_source_default_to_unsupported() {
        use engine_core::error::FailureClass;

        let message = Message::new(
            MessageId::try_from("eaaaaab").unwrap(),
            Memberships::of_one(MailboxId::try_from("a").unwrap()),
        );
        // A mail adapter that did not override body fetch rejects, so a
        // capability-checking caller never depends on the default — and a boxed
        // adapter delegates `fetch_message_source` to that same default.
        let direct = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        let boxed: Box<dyn Provider> = Box::new(FakeJmap {
            caps: Capabilities::none().with_mail(),
        });
        for err in [
            direct
                .fetch_message_source(&account(), &message)
                .await
                .unwrap_err(),
            boxed
                .fetch_message_source(&account(), &message)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(err.class(), FailureClass::InvalidState);
        }
    }

    #[tokio::test]
    async fn calendar_writes_default_to_unsupported() {
        use engine_core::error::FailureClass;
        use engine_core::ids::{EventId, Uid};
        use engine_core::raw::RawIcal;

        let provider = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        let href = EventId::try_from("/cal/evt-1.ics").unwrap();
        let write = crate::EventWrite::create(
            href.clone(),
            Uid::new("evt-1@host").unwrap(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        );
        // A provider that did not override calendar writes rejects, so a
        // capability-checking caller never depends on the default.
        let err = provider.put_event(&account(), &write).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::InvalidState);
        let deletion = crate::EventDeletion::unconditional(href);
        let err = provider
            .delete_event(&account(), &deletion)
            .await
            .unwrap_err();
        assert_eq!(err.class(), FailureClass::InvalidState);
    }

    #[tokio::test]
    async fn calendar_methods_default_to_unsupported_with_jmap_scopes() {
        let provider = FakeJmap {
            caps: Capabilities::none().with_mail(),
        };
        assert_eq!(
            provider.calendar_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::Calendar,
            }
        );
        assert_eq!(
            provider.event_scope(&account()),
            SyncScope::JmapType {
                account: account(),
                data_type: JmapDataType::CalendarEvent,
            }
        );
        assert!(provider.sync_calendars(&account(), None).await.is_err());
        assert!(provider.sync_events(&account(), None).await.is_err());
    }

    #[test]
    fn provider_is_object_safe() {
        // Hosts may hold `Box<dyn Provider>`; ensure the trait stays object-safe.
        let _provider: Box<dyn Provider> = Box::new(FakeJmap {
            caps: Capabilities::none().with_mail(),
        });
    }
}
