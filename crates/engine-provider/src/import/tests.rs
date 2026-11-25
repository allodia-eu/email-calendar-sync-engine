use std::{
    collections::BTreeSet,
    num::{NonZeroU64, NonZeroUsize},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::SyncState,
    time::UtcDateTime,
};

use super::{ImportedMessage, MailImportLimits, MailImporter, MessageImport};
use crate::{ProviderError, ProviderResult};

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

fn message_of(bytes: usize, mailboxes: &[&str]) -> MessageImport {
    MessageImport::new(
        RawMime::new(vec![b'x'; bytes]),
        Memberships::new(mailboxes.iter().copied().map(mailbox)).unwrap(),
    )
}

#[test]
fn a_message_is_stored_as_the_server_stamps_it_unless_told_otherwise() {
    let message = message_of(3, &["inbox"]);

    assert_eq!(message.metadata, MessageMetadata::default());
    assert_eq!(message.metadata.keywords, BTreeSet::new());
    assert_eq!(message.metadata.received_at, None);

    let received = "2024-03-01T08:15:00Z".parse::<UtcDateTime>().unwrap();
    let seen = BTreeSet::from([Keyword::new("$seen").unwrap()]);
    let message = message.with_metadata(
        MessageMetadata::default()
            .with_keywords(seen.clone())
            .with_received_at(received),
    );

    assert_eq!(message.metadata.keywords, seen);
    assert_eq!(message.metadata.received_at, Some(received));
}

#[test]
fn a_message_never_prints_its_bytes() {
    let message = MessageImport::new(
        RawMime::new(b"Subject: private\r\n\r\nbody".to_vec()),
        Memberships::of_one(mailbox("inbox")),
    );

    let printed = format!("{message:?}");

    assert!(!printed.contains("private"), "{printed}");
}

#[test]
fn a_receipt_says_whether_the_target_already_held_the_message() {
    let created = ImportedMessage::created(Some(key("u7")));
    let existing = ImportedMessage::existing(key("e1"));

    assert_eq!(created.key, Some(key("u7")));
    assert!(!created.already_there);
    assert_eq!(created.kept, MessageMetadata::default());
    assert_eq!(existing.key, Some(key("e1")));
    assert!(existing.already_there);
    assert_eq!(existing.kept, MessageMetadata::default());
}

#[test]
fn a_receipt_says_what_metadata_the_target_kept() {
    let kept =
        MessageMetadata::default().with_keywords(BTreeSet::from([Keyword::new("$seen").unwrap()]));

    let receipt = ImportedMessage::created(None).with_kept(kept.clone());

    assert_eq!(receipt.kept, kept);
}

#[test]
fn limits_admit_a_message_that_fits() {
    let limits = MailImportLimits::new(NonZeroUsize::new(2).unwrap())
        .with_max_message_size(NonZeroU64::new(10).unwrap());

    assert_eq!(limits.max_mailboxes().get(), 2);
    assert_eq!(limits.max_message_size(), NonZeroU64::new(10));
    assert!(limits.check(&message_of(10, &["inbox", "work"])).is_ok());
}

#[test]
fn limits_refuse_a_message_over_the_size_the_target_states_as_that_message_alone() {
    let limits = MailImportLimits::new(NonZeroUsize::MIN)
        .with_max_message_size(NonZeroU64::new(10).unwrap());

    let refusal = limits.check(&message_of(11, &["inbox"])).unwrap_err();

    assert_eq!(refusal.class(), FailureClass::Permanent);
    assert!(refusal.is_item_unavailable(), "{refusal}");
}

#[test]
fn limits_refuse_a_message_filed_in_more_mailboxes_than_the_target_allows() {
    let limits = MailImportLimits::new(NonZeroUsize::MIN);

    let refusal = limits
        .check(&message_of(1, &["inbox", "work"]))
        .unwrap_err();

    assert_eq!(refusal.class(), FailureClass::Permanent);
    assert!(refusal.is_item_unavailable(), "{refusal}");
}

#[test]
fn a_target_that_states_no_size_admits_any_size() {
    let limits = MailImportLimits::new(NonZeroUsize::MIN);

    assert_eq!(limits.max_message_size(), None);
    assert!(limits.check(&message_of(1 << 20, &["inbox"])).is_ok());
}

/// An importer that answers from the limits alone, as the shape every adapter takes: one answer
/// per message, in order.
struct Recording {
    limits: MailImportLimits,
    stored: Mutex<Vec<usize>>,
}

#[async_trait]
impl MailImporter for Recording {
    async fn mail_import_limits(&self, _: &AccountId) -> ProviderResult<MailImportLimits> {
        Ok(self.limits)
    }

    async fn import_cursor(&self, _: &AccountId, _: &MailboxId) -> ProviderResult<SyncState> {
        Ok(SyncState::new("n1"))
    }

    async fn import_messages(
        &self,
        _: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        batch
            .iter()
            .map(|message| {
                self.limits.check(message)?;
                let mut stored = self.stored.lock().unwrap();
                stored.push(message.raw.len());
                Ok(ImportedMessage::created(Some(key(&format!(
                    "u{}",
                    stored.len()
                )))))
            })
            .collect()
    }
}

#[tokio::test]
async fn an_importer_answers_each_message_of_a_batch_in_order_through_a_trait_object() {
    let importer: Box<dyn MailImporter> = Box::new(Recording {
        limits: MailImportLimits::new(NonZeroUsize::MIN)
            .with_max_message_size(NonZeroU64::new(5).unwrap()),
        stored: Mutex::new(Vec::new()),
    });
    let batch = [
        message_of(5, &["inbox"]),
        message_of(6, &["inbox"]),
        message_of(1, &["inbox"]),
    ];

    let answers = importer.import_messages(&account(), &batch).await;

    assert_eq!(answers.len(), batch.len());
    assert_eq!(answers[0].as_ref().unwrap().key, Some(key("u1")));
    assert!(
        answers[1]
            .as_ref()
            .is_err_and(ProviderError::is_item_unavailable)
    );
    assert_eq!(answers[2].as_ref().unwrap().key, Some(key("u2")));
    assert_eq!(
        importer
            .import_cursor(&account(), &mailbox("inbox"))
            .await
            .unwrap(),
        SyncState::new("n1")
    );
}

/// Calls every method of `importer`, as code generic over [`MailImporter`] does.
async fn drive<I: MailImporter + ?Sized>(importer: &I) -> (MailImportLimits, SyncState, usize) {
    let limits = importer.mail_import_limits(&account()).await.unwrap();
    let cursor = importer
        .import_cursor(&account(), &mailbox("inbox"))
        .await
        .unwrap();
    let answers = importer
        .import_messages(&account(), &[message_of(1, &["inbox"])])
        .await;
    (limits, cursor, answers.len())
}

fn recording() -> Recording {
    Recording {
        limits: MailImportLimits::new(NonZeroUsize::MIN)
            .with_max_message_size(NonZeroU64::new(5).unwrap()),
        stored: Mutex::new(Vec::new()),
    }
}

#[tokio::test]
async fn a_boxed_or_shared_importer_delegates_every_method_to_the_one_inside() {
    let expected = (recording().limits, SyncState::new("n1"), 1);
    let boxed: Box<dyn MailImporter> = Box::new(recording());
    assert_eq!(drive(&boxed).await, expected);
    let shared: Arc<dyn MailImporter> = Arc::new(recording());
    assert_eq!(drive(&shared).await, expected);
    assert_eq!(drive(&Arc::new(recording())).await, expected);
}

/// A provider that can import, as an adapter that implements both traits is.
struct ImportingProvider {
    provider: crate::tests::FakeJmap,
    importer: Recording,
}

#[async_trait]
impl crate::Provider for ImportingProvider {
    fn connection_info(&self) -> crate::ConnectionInfo {
        self.provider.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> engine_core::sync::SyncScope {
        self.provider.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> engine_core::sync::SyncScope {
        self.provider.email_scope(account)
    }

    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<crate::ScopeSync<engine_core::mail::Mailbox>> {
        self.provider.sync_mailboxes(account, cursor).await
    }

    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: engine_core::sync::MailboxWindows,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> crate::EmailStream<'a> {
        self.provider
            .stream_email(account, cursor, window, fetch_batch, chunk_size)
    }

    fn mail_importer(&self) -> Option<&dyn MailImporter> {
        Some(self)
    }
}

impl crate::CalendarSync for ImportingProvider {}
impl crate::MailboxWrites for ImportingProvider {}
impl crate::CalendarWrites for ImportingProvider {}

#[async_trait]
impl MailImporter for ImportingProvider {
    async fn mail_import_limits(&self, account: &AccountId) -> ProviderResult<MailImportLimits> {
        self.importer.mail_import_limits(account).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState> {
        self.importer.import_cursor(account, mailbox).await
    }

    async fn import_messages(
        &self,
        account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        self.importer.import_messages(account, batch).await
    }
}

fn importer_of<P: crate::Provider + ?Sized>(provider: &P) -> Option<&dyn MailImporter> {
    provider.mail_importer()
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_its_importer_through_the_accessor() {
    let held: Box<dyn crate::Provider> = Box::new(ImportingProvider {
        provider: crate::tests::FakeJmap {
            info: crate::ConnectionInfo::new(crate::Capabilities::none().with_mail()),
        },
        importer: recording(),
    });

    // Through code generic over `P: Provider`, as a host's engine calls take it, which is the
    // `Box<dyn Provider>` impl's delegation.
    let importer = importer_of(&held).expect("an importing adapter answers itself");

    assert_eq!(
        drive(importer).await,
        (recording().limits, SyncState::new("n1"), 1)
    );
}

#[test]
fn a_provider_that_cannot_import_answers_none() {
    let plain: Box<dyn crate::Provider> = Box::new(crate::tests::FakeJmap {
        info: crate::ConnectionInfo::new(crate::Capabilities::none().with_mail()),
    });

    assert!(importer_of(&plain).is_none());
}

/// A message the batch never sent carries a throttle (with its wait) or a refused credential,
/// which reach every message; any other ending was about the exchange or another message, so
/// the unsent one is plainly retryable, and never may be stored or unavailable.
#[test]
fn a_message_the_batch_never_sent_carries_the_batch_s_reason_and_wait() {
    let wait = Some(std::time::Duration::from_secs(1206));
    let throttled = super::not_sent(&ProviderError::rate_limited("x", wait.map(Into::into)));
    assert_eq!(throttled.class(), FailureClass::RateLimited);
    assert_eq!(throttled.retry_after(), wait.map(Into::into));

    let refused = super::not_sent(&ProviderError::authentication("x"));
    assert_eq!(refused.class(), FailureClass::Authentication);

    for about_one_message in [
        ProviderError::needs_confirmation("x"),
        ProviderError::item_unavailable("x"),
    ] {
        let unsent = super::not_sent(&about_one_message);
        assert_eq!(unsent.class(), FailureClass::Retryable, "{unsent}");
        assert!(!unsent.requires_confirmation() && !unsent.is_item_unavailable());
    }
    let unsent = super::not_sent(&ProviderError::permanent("a secret subject"));
    assert_eq!(
        unsent.class(),
        FailureClass::Retryable,
        "about the exchange, not this message"
    );
    assert!(unsent.detail().starts_with("not sent"), "{unsent}");
    assert!(!unsent.detail().contains("secret"), "{unsent}");
}

/// A batch ended because no credential could be had says so on every message it never sent,
/// with the source's class and wait, and does not blame the target.
#[test]
fn a_message_never_sent_for_want_of_a_credential_says_so() {
    let wait = Some(std::time::Duration::from_secs(30));
    for ending in [
        ProviderError::authentication("consent is gone").of_credential(),
        ProviderError::rate_limited("token endpoint 429", wait.map(Into::into)).of_credential(),
        // A credential that may not go there, or cannot be presented: never sendable as it is.
        ProviderError::permanent("not permitted").of_credential(),
        // A source answering for another account.
        ProviderError::invalid_state("another account").of_credential(),
    ] {
        let unsent = super::not_sent(&ending);
        assert!(unsent.is_credential_failure(), "{unsent}");
        assert_eq!(unsent.class(), ending.class());
        assert_eq!(unsent.retry_after(), ending.retry_after());
        assert!(!unsent.detail().contains("target"), "{unsent}");
        assert!(super::not_sent(&unsent).is_credential_failure(), "{unsent}");
    }
    assert!(!super::not_sent(&ProviderError::authentication("401")).is_credential_failure());
}

/// A batch ends on what reaches every item after it: a throttle, a refused credential, or no
/// credential at all, whatever the source's class. Nothing about one item or one exchange ends
/// it.
#[test]
fn a_batch_ends_on_a_throttle_a_refused_credential_or_no_credential() {
    for ending in [
        ProviderError::rate_limited("429", None),
        ProviderError::authentication("401"),
        ProviderError::retryable("the broker is down").of_credential(),
        ProviderError::permanent("not permitted").of_credential(),
    ] {
        assert!(super::ends_batch(&ending), "{ending:?}");
    }
    for one in [
        ProviderError::retryable("connection reset"),
        ProviderError::item_unavailable("too large"),
        ProviderError::needs_confirmation("lost"),
        ProviderError::permanent("403"),
    ] {
        assert!(!super::ends_batch(&one), "{one:?}");
    }
}
