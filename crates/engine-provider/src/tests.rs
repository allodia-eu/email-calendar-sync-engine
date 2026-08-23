//! Behavior tests for the provider trait: scope defaults, the paged-drain default,
//! capability-gated rejections, and the `Box<dyn Provider>` blanket impl delegating
//! to both overrides and defaults.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageId},
    mail::{Mailbox, Message},
    membership::Memberships,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
};

use super::*;

/// A trivial in-memory provider, proving the trait is implementable and that
/// the scope accessors + connection info + ScopeSync compose as intended.
struct FakeJmap {
    info: ConnectionInfo,
}

#[async_trait]
impl Provider for FakeJmap {
    fn connection_info(&self) -> ConnectionInfo {
        self.info
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

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let msg = Message::new(
            MessageId::try_from("eaaaaab").unwrap(),
            Memberships::of_one(MailboxId::try_from("a").unwrap()),
        );
        let key = msg.id.key().clone();
        // A first sync (no cursor) is a one-chunk reconciling snapshot (so the drain
        // covers the tombstone path); a later one is an additive empty delta.
        let chunk = if cursor.is_none() {
            EmailChunk::reconcile_last(vec![msg], vec![key], Some(1), SyncState::new("email-2"))
        } else {
            EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("email-2"))
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

#[tokio::test]
async fn provider_returns_scoped_updates_and_cursors() {
    let provider = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    assert!(provider.connection_info().capabilities.mail());
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
async fn email_stream_primitive_drives_the_drain_default() {
    use futures_util::StreamExt;

    let provider = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };

    // The streaming primitive: a first pass (no cursor) is a one-chunk reconciling
    // snapshot that carries the ids it covers, its progress total, and the cursor.
    let chunks: Vec<_> = provider
        .stream_email(&account(), None, SyncWindow::full(), 50, 0)
        .collect()
        .await;
    assert_eq!(chunks.len(), 1);
    let chunk = chunks.into_iter().next().unwrap().unwrap();
    assert_eq!(chunk.mode, PassMode::Reconcile);
    assert_eq!(chunk.total, Some(1));
    assert!(chunk.is_reconcile_final());
    assert_eq!(chunk.present.len(), 1);
    assert_eq!(chunk.advance_to.as_ref().unwrap().as_str(), "email-2");

    // The default drain merges the chunk(s) back into one snapshot update,
    // advancing to the final chunk's cursor — adapters implement only streaming.
    let drained = provider.sync_email(&account(), None).await.unwrap();
    assert!(drained.is_snapshot());
    assert_eq!(drained.next_cursor.as_str(), "email-2");
}

#[tokio::test]
async fn submit_email_defaults_to_unsupported() {
    use engine_core::{error::FailureClass, ids::MessageIdHeader, mail::EmailAddress};

    let provider = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
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

/// A provider implementing only the required `connection_info`, leaving every other
/// method to its trait default — so boxing it exercises the blanket impl's
/// delegation to the *defaults*, not just to an adapter's overrides.
struct BareProvider {
    info: ConnectionInfo,
}

impl Provider for BareProvider {
    fn connection_info(&self) -> ConnectionInfo {
        self.info
    }
}

#[tokio::test]
async fn box_dyn_provider_delegates_overrides_and_defaults() {
    use engine_core::{error::FailureClass, ids::MessageIdHeader, mail::EmailAddress};
    use futures_util::StreamExt;

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
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    assert!(over.connection_info().capabilities.mail());
    assert_eq!(over.email_scope(&account()), email_scope);
    assert_eq!(over.mailbox_scope(&account()), mailbox_scope);
    assert!(over.sync_mailboxes(&account(), None).await.is_ok());
    let chunks: Vec<_> = over
        .stream_email(&account(), None, SyncWindow::full(), 50, 0)
        .collect()
        .await;
    let chunk = chunks.into_iter().next().unwrap().unwrap();
    assert_eq!(chunk.mode, PassMode::Reconcile);
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
        info: ConnectionInfo::new(Capabilities::none()),
    });
    assert!(!bare.connection_info().capabilities.mail());
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
    // The bare stream default yields a single classified `Err`, so a
    // capability-checking caller never relies on it.
    let stream_first = bare
        .stream_email(&account(), None, SyncWindow::full(), 0, 0)
        .next()
        .await
        .unwrap()
        .unwrap_err();
    let rejected = [
        bare.sync_mailboxes(&account(), None).await.unwrap_err(),
        stream_first,
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
    for class in calendar_write_rejections(&bare).await {
        assert_eq!(class, FailureClass::InvalidState);
    }
}

/// A stored event as a sync hands it back — the base every edit and delete is built from.
fn stored_event() -> engine_core::calendar::Event {
    use engine_core::{
        ids::{CalendarId, EventId, ProviderKey, Uid},
        time::{CalendarDateTime, LocalDateTime, TimeZoneId},
    };

    engine_core::calendar::Event::new(
        EventId::try_from("/cal/e.ics").unwrap(),
        Uid::new("e@host").unwrap(),
        Memberships::of_one(CalendarId::new(ProviderKey::new("/cal/").unwrap())),
        CalendarDateTime::Zoned {
            local: "2026-08-01T09:00:00".parse::<LocalDateTime>().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
    )
}

/// Drives every calendar write verb against `provider` and returns how each failed.
async fn calendar_write_rejections<P: Provider>(
    provider: &P,
) -> Vec<engine_core::error::FailureClass> {
    use engine_core::{
        ids::{CalendarId, ProviderKey},
        raw::RawIcal,
    };

    use crate::{EventDeletion, EventDraft, EventEdit, EventPatch, EventWrite, PatchTarget};

    let base = stored_event();
    let stamp = "2026-07-14T10:00:00Z".parse().unwrap();
    let draft = EventDraft::new(
        CalendarId::new(ProviderKey::new("/cal/").unwrap()),
        base.uid.clone(),
        "Standup",
        base.start.clone(),
        base.start.clone(),
        stamp,
    );
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp).summary("Renamed"),
    );
    let write = EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"));

    vec![
        provider
            .create_event(&account(), &draft)
            .await
            .unwrap_err()
            .class(),
        provider
            .patch_event(&account(), &base, &edit)
            .await
            .unwrap_err()
            .class(),
        provider
            .put_event(&account(), &write)
            .await
            .unwrap_err()
            .class(),
        provider
            .delete_event(&account(), Some(&base), &EventDeletion::of(&base))
            .await
            .unwrap_err()
            .class(),
    ]
}

#[tokio::test]
async fn mail_writes_default_to_unsupported() {
    use engine_core::{error::FailureClass, ids::ProviderKey};

    let edit = crate::MailEdit::delete(ProviderKey::new("imap:v1:u7@INBOX").unwrap());
    // A mail adapter that did not override writes rejects, so a
    // capability-checking caller never depends on the default — and a boxed
    // adapter delegates `edit_mail` to that same default (the blanket impl).
    let direct = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    let boxed: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
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
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    let boxed: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
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

    // Every calendar write verb — the neutral create/patch/delete spine *and* the
    // document-replace escape hatch — rejects on a provider that did not override it, so a
    // capability-checking caller never depends on the default. A boxed adapter delegates to
    // that same default through the blanket impl.
    let direct = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    };
    let boxed: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    for class in calendar_write_rejections(&direct)
        .await
        .into_iter()
        .chain(calendar_write_rejections(&boxed).await)
    {
        assert_eq!(class, FailureClass::InvalidState);
    }
}

#[tokio::test]
async fn calendar_methods_default_to_unsupported_with_jmap_scopes() {
    let provider = FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
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
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
}

#[tokio::test]
async fn box_dyn_provider_delegates_the_transport_facts_not_just_capabilities() {
    // A host behind dynamic dispatch must still see the *whole* post-connect object:
    // if the blanket impl ever rebuilt a `ConnectionInfo` from capabilities alone,
    // the negotiated versions would silently become `None`.
    let inner = FakeJmap {
        info: ConnectionInfo {
            tls_version: Some(TlsVersion::Tls1_3),
            http_version: Some(HttpVersion::Http2),
            ..ConnectionInfo::new(Capabilities::none().with_mail())
        },
    };
    let expected = inner.connection_info();
    let boxed: Box<dyn Provider> = Box::new(inner);
    assert_eq!(boxed.connection_info(), expected);
    assert_eq!(
        boxed.connection_info().tls_version,
        Some(TlsVersion::Tls1_3)
    );
    assert_eq!(
        boxed.connection_info().http_version,
        Some(HttpVersion::Http2)
    );
}
