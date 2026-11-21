use std::{
    collections::BTreeSet,
    num::{NonZeroU64, NonZeroUsize},
    sync::Mutex,
};

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Keyword,
    membership::Memberships,
    raw::RawMime,
    sync::SyncState,
    time::UtcDateTime,
};

use super::{ImportedMessage, MailImportLimits, MailImporter, MessageImport, MessageMetadata};
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
