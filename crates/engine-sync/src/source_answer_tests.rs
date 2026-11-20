//! Every road that holds a message's source answers its paperclip, once.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageId},
    mail::{Message, MessageBody},
    membership::Memberships,
    raw::RawMime,
};
use engine_provider::{CalendarWrites, Capabilities, ConnectionInfo, Provider, ProviderResult};
use engine_store::{ManualClock, MessageBodyStore, MessageSourceCache};
use store_sqlite::SqliteStore;

use crate::{
    ensure_message_source, fetch_inline_parts, fetch_message_attachment, fetch_message_body,
    fetch_message_scheduling, fetch_message_source,
};

/// A source with an attached PDF.
const WITH_FILE: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
    --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
    --m\r\nContent-Type: application/pdf\r\n\
    Content-Disposition: attachment; filename=\"a.pdf\"\r\n\r\nPDF\r\n--m--\r\n";

/// A provider that hands every message the same source.
struct Source(&'static [u8]);

#[async_trait]
impl Provider for Source {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_message_source())
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        _message: &Message,
    ) -> ProviderResult<RawMime> {
        Ok(RawMime::new(self.0.to_vec()))
    }
}

impl engine_provider::MailboxWrites for Source {}
impl CalendarWrites for Source {}

fn account() -> AccountId {
    AccountId::try_from("acct").expect("account")
}

fn message() -> Message {
    Message::new(
        MessageId::try_from("imap:v1:u1@INBOX").expect("id"),
        Memberships::of_one(MailboxId::try_from("INBOX").expect("mailbox")),
    )
}

fn store() -> SqliteStore<ManualClock> {
    SqliteStore::open_in_memory(ManualClock::new(
        "2026-06-26T00:00:00Z".parse().expect("instant"),
    ))
    .expect("store")
}

async fn answer(store: &SqliteStore<ManualClock>) -> Option<bool> {
    store
        .mail_has_attachment(&account(), message().id.key())
        .await
        .expect("read")
}

#[tokio::test]
async fn every_road_that_fetches_a_source_answers_it() {
    // Not only the open: an invitation card, an inline image, an export or a warm can be the
    // first thing to fetch a message's bytes, and then no later open re-reads them.
    let provider = Source(WITH_FILE);

    let store_a = store();
    fetch_inline_parts(&provider, &store_a, &account(), &message())
        .await
        .unwrap();
    assert_eq!(answer(&store_a).await, Some(true), "inline parts");

    let store_b = store();
    ensure_message_source(&provider, &store_b, &account(), &message())
        .await
        .unwrap();
    assert_eq!(answer(&store_b).await, Some(true), "ensure source");

    let store_c = store();
    fetch_message_source(&provider, &store_c, &account(), &message())
        .await
        .unwrap();
    assert_eq!(answer(&store_c).await, Some(true), "export");

    let store_d = store();
    let _ = fetch_message_scheduling(&provider, &store_d, &account(), &message()).await;
    assert_eq!(answer(&store_d).await, Some(true), "scheduling");

    let store_e = store();
    let _ = fetch_message_attachment(
        &provider,
        &store_e,
        &account(),
        &message(),
        engine_core::mail::AttachmentPartId::new(2),
    )
    .await;
    assert_eq!(answer(&store_e).await, Some(true), "one attachment");
}

#[tokio::test]
async fn a_body_cached_before_answers_were_recorded_is_answered_from_its_cached_source() {
    // An upgrade leaves warm messages with body text and bytes cached and no answer, and the
    // fast path never reads the bytes. The first open pays the answer from the blob, once.
    let store = store();
    store
        .put_message_source(
            &account(),
            message().id.key(),
            RawMime::new(WITH_FILE.to_vec()),
        )
        .await
        .unwrap();
    store
        .put_message_body(
            &account(),
            message().id.key(),
            &MessageBody::new(Some("body".to_owned()), None),
        )
        .await
        .unwrap();
    assert_eq!(answer(&store).await, None);

    // A provider that cannot be reached: the answer must come from the cache alone.
    let unreachable = Source(b"");
    fetch_message_body(&unreachable, &store, &account(), &message())
        .await
        .unwrap();
    assert_eq!(answer(&store).await, Some(true));
}

#[tokio::test]
async fn a_source_that_is_no_message_answers_nothing() {
    // An empty or broken source says nothing about the message, so the provider's flag stays.
    let store = store();
    fetch_message_body(&Source(b""), &store, &account(), &message())
        .await
        .unwrap();
    assert_eq!(answer(&store).await, None);
}
