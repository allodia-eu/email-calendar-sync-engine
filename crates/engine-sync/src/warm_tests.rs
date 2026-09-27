//! Tests for [`warm_message_sources`].

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageId},
    mail::Message,
    membership::Memberships,
    raw::RawMime,
};
use engine_provider::{
    CalendarWrites, Capabilities, ConnectionInfo, Provider, ProviderError, ProviderResult,
    SourceStream,
};
use engine_store::{ManualClock, MessageBodyStore, MessageSourceCache};
use futures_util::StreamExt;
use store_sqlite::SqliteStore;

use super::warm_message_sources;

/// Answers a batch in reverse, as a server streaming rows in its own order may, and fails the
/// message keyed `u2`.
struct BatchProvider;

#[async_trait]
impl Provider for BatchProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_message_source())
            .with_sources_per_request(10)
    }

    fn fetch_message_sources<'a>(
        &'a self,
        _account: &'a AccountId,
        messages: &'a [Message],
    ) -> SourceStream<'a> {
        let answers: Vec<_> = messages
            .iter()
            .enumerate()
            .rev()
            .map(|(index, message)| {
                let key = message.id.key().as_str();
                let answer = if key.contains(":u2@") {
                    Err(ProviderError::conflict("expunged"))
                } else {
                    Ok(RawMime::new(
                        format!("Content-Type: text/plain\r\n\r\nbody of {key}").into_bytes(),
                    ))
                };
                (index, answer)
            })
            .collect();
        Box::pin(futures_util::stream::iter(answers))
    }
}

impl CalendarWrites for BatchProvider {}

/// Implements only the single fetch, so the batch runs through the trait's default.
struct SingleProvider;

#[async_trait]
impl Provider for SingleProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_message_source())
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        message: &Message,
    ) -> ProviderResult<RawMime> {
        Ok(RawMime::new(
            format!(
                "Content-Type: text/plain\r\n\r\none of {}",
                message.id.key()
            )
            .into_bytes(),
        ))
    }
}

impl CalendarWrites for SingleProvider {}

fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

fn message(uid: u32) -> Message {
    Message::new(
        MessageId::try_from(format!("imap:v1:u{uid}@INBOX").as_str()).unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

fn store() -> SqliteStore<ManualClock> {
    SqliteStore::open_in_memory(ManualClock::new("2026-06-26T00:00:00Z".parse().unwrap())).unwrap()
}

#[tokio::test]
async fn every_message_that_arrives_is_cached_and_a_failure_is_its_own() {
    let store = store();
    let messages = [message(1), message(2), message(3)];

    let mut outcomes: Vec<_> = warm_message_sources(&BatchProvider, &store, &account(), &messages)
        .map(|(index, outcome)| {
            (
                index,
                outcome.map_err(|err| match err {
                    crate::SyncError::Provider(err) => err.class(),
                    other => panic!("not a provider failure: {other}"),
                }),
            )
        })
        .collect()
        .await;
    outcomes.sort_by_key(|(index, _)| *index);

    assert_eq!(
        outcomes,
        vec![(0, Ok(())), (1, Err(FailureClass::Conflict)), (2, Ok(()))]
    );
    for warmed in [&messages[0], &messages[2]] {
        let key = warmed.id.key();
        assert!(
            store
                .get_message_source(&account(), key)
                .await
                .unwrap()
                .is_some()
        );
        let body = store
            .get_message_body(&account(), key)
            .await
            .unwrap()
            .unwrap();
        assert!(body.plain().unwrap().contains(key.as_str()));
    }
    let failed = messages[1].id.key();
    assert!(
        store
            .get_message_source(&account(), failed)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_provider_without_batches_warms_one_message_at_a_time() {
    let store = store();
    let messages = [message(1), message(2)];

    let outcomes: Vec<_> = warm_message_sources(&SingleProvider, &store, &account(), &messages)
        .map(|(index, outcome)| (index, outcome.is_ok()))
        .collect()
        .await;

    assert_eq!(outcomes, vec![(0, true), (1, true)]);
    let body = store
        .get_message_body(&account(), messages[1].id.key())
        .await
        .unwrap()
        .unwrap();
    assert!(body.plain().unwrap().contains("u2@INBOX"));
}
