//! Bodies read over IMAP: each handed to the message it belongs to whatever order Gmail sends
//! them in, a message no folder holds reported as gone, and a failure reaching every message
//! not yet answered.

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::Message,
    membership::Memberships,
    raw::RawMime,
};
use engine_provider::{Provider, ProviderError, ProviderResult};
use futures_util::StreamExt;
use provider_imap::{
    GmailFolder, GmailFolderState, GmailMessage, GmailQuery, GmailSources, GmailState,
};

use crate::{GmailProvider, imap_mail::ImapMail, test_support::fake_client};

/// Hands out `bodies` in the order given, then fails with `then_fail` if set.
struct Bodies {
    bodies: Vec<(u64, &'static str)>,
    then_fail: Option<FailureClass>,
}

#[async_trait]
impl ImapMail for Bodies {
    async fn open(&self, _: GmailFolder) -> ProviderResult<Option<GmailFolderState>> {
        Ok(None)
    }

    async fn search(&self, _: GmailFolder, _: u32, _: &GmailQuery) -> ProviderResult<Vec<u32>> {
        Ok(Vec::new())
    }

    async fn messages(
        &self,
        _: GmailFolder,
        _: u32,
        _: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>> {
        Ok(Vec::new())
    }

    async fn changed(
        &self,
        _: GmailFolder,
        _: u32,
        _: u32,
        _: u64,
    ) -> ProviderResult<Vec<GmailState>> {
        Ok(Vec::new())
    }

    async fn ids(&self, _: GmailFolder, _: u32, _: &[u32]) -> ProviderResult<Vec<(u32, u64)>> {
        Ok(Vec::new())
    }

    fn sources<'a>(&'a self, gmail_ids: &'a [u64]) -> GmailSources<'a> {
        let mut items: Vec<ProviderResult<(u64, RawMime)>> = self
            .bodies
            .iter()
            .filter(|(id, _)| gmail_ids.contains(id))
            .map(|(id, body)| Ok((*id, RawMime::new(body.as_bytes().to_vec()))))
            .collect();
        if let Some(class) = self.then_fail {
            items.push(Err(ProviderError::new(class, "the connection dropped")));
        }
        Box::pin(futures_util::stream::iter(items))
    }
}

fn provider(bodies: Vec<(u64, &'static str)>, then_fail: Option<FailureClass>) -> GmailProvider {
    GmailProvider::new(fake_client(Vec::new()))
        .with_imap_mail(Arc::new(Bodies { bodies, then_fail }))
}

fn message(key: &str) -> Message {
    Message::new(
        MessageId::new(ProviderKey::new(key).unwrap()),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    )
}

fn account() -> AccountId {
    AccountId::try_from("a").unwrap()
}

#[tokio::test]
async fn each_body_reaches_its_own_message_and_a_missing_one_is_reported_gone() {
    // Gmail ids 0xa, 0xb and 0xc; the server sends c before a and holds no b.
    let provider = provider(vec![(0xc, "third"), (0xa, "first")], None);
    let messages = [message("a"), message("b"), message("c")];
    let mut got: Vec<(usize, ProviderResult<RawMime>)> = provider
        .fetch_message_sources(&account(), &messages)
        .collect()
        .await;
    got.sort_by_key(|(index, _)| *index);
    assert_eq!(got[0].1.as_ref().unwrap().as_bytes(), b"first");
    assert_eq!(
        got[1].1.as_ref().unwrap_err().class(),
        FailureClass::Conflict
    );
    assert_eq!(got[2].1.as_ref().unwrap().as_bytes(), b"third");
}

#[tokio::test]
async fn a_failure_reaches_every_message_not_yet_answered() {
    let provider = provider(vec![(0xa, "first")], Some(FailureClass::Retryable));
    let messages = [message("a"), message("b"), message("not-hex")];
    let mut got: Vec<(usize, ProviderResult<RawMime>)> = provider
        .fetch_message_sources(&account(), &messages)
        .collect()
        .await;
    got.sort_by_key(|(index, _)| *index);
    assert_eq!(got.len(), 3, "each message is answered exactly once");
    assert!(got[0].1.is_ok());
    assert_eq!(
        got[1].1.as_ref().unwrap_err().class(),
        FailureClass::Retryable
    );
    assert_eq!(
        got[2].1.as_ref().unwrap_err().class(),
        FailureClass::Retryable
    );
}

#[tokio::test]
async fn one_body_is_read_the_same_way() {
    let gmail = provider(vec![(0xa, "first")], None);
    let source = gmail
        .fetch_message_source(&account(), &message("a"))
        .await
        .unwrap();
    assert_eq!(source.as_bytes(), b"first");
    let gone = gmail
        .fetch_message_source(&account(), &message("b"))
        .await
        .unwrap_err();
    assert_eq!(gone.class(), FailureClass::Conflict);
    let bad = gmail
        .fetch_message_source(&account(), &message("not-hex"))
        .await
        .unwrap_err();
    assert_eq!(bad.class(), FailureClass::InvalidState);

    let failing = provider(Vec::new(), Some(FailureClass::RateLimited));
    let err = failing
        .fetch_message_source(&account(), &message("a"))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::RateLimited);
}
