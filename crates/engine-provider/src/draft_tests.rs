//! Unit tests for the draft verbs' rejecting defaults
//! ([`Provider::put_draft`](super::Provider::put_draft) /
//! [`delete_draft`](super::Provider::delete_draft)). A sibling of `provider.rs` so it
//! stays under the line limit.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword},
};

use crate::{
    Capabilities, ConnectionInfo, Draft, DraftReceipt, Provider, ProviderResult,
    tests::{FakeJmap, account},
};

fn none() -> BTreeSet<Keyword> {
    BTreeSet::new()
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("gen-1@host").unwrap(),
        EmailAddress::new("a@host"),
        vec![EmailAddress::new("b@host")],
        "Hi",
        "body",
    )
}

fn mail_only() -> FakeJmap {
    FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    }
}

#[tokio::test]
async fn storing_a_draft_defaults_to_unsupported() {
    // An adapter that did not override the draft verbs rejects both, so a
    // capability-checking caller never depends on the default — and a boxed adapter
    // delegates to that same default (the blanket impl).
    let boxed: Box<dyn Provider> = Box::new(mail_only());
    let key = ProviderKey::new("imap:v1:u7@Drafts").unwrap();
    for err in [
        mail_only()
            .put_draft(&account(), &draft(), None, &none())
            .await
            .unwrap_err(),
        boxed
            .put_draft(&account(), &draft(), Some(&key), &none())
            .await
            .unwrap_err(),
        mail_only()
            .delete_draft(&account(), &key)
            .await
            .unwrap_err(),
        boxed.delete_draft(&account(), &key).await.unwrap_err(),
    ] {
        assert_eq!(err.class(), FailureClass::InvalidState);
    }
}

/// An adapter that can store a draft, answering with a key naming what it was asked and
/// keeping every keyword.
/// The point is the **boxed** path: `Box<dyn Provider>` forwards each verb explicitly,
/// so a verb nobody forwarded silently falls through to the rejecting default above.
/// Two adapters that both reject cannot tell those apart, so this one succeeds.
struct FakeDrafts;

impl crate::MailboxWrites for FakeDrafts {}
impl crate::CalendarWrites for FakeDrafts {}

#[async_trait::async_trait]
impl Provider for FakeDrafts {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_mail_drafts())
    }

    async fn put_draft(
        &self,
        _account: &AccountId,
        _draft: &Draft,
        replacing: Option<&ProviderKey>,
        keywords: &BTreeSet<Keyword>,
    ) -> ProviderResult<DraftReceipt> {
        let placed = match replacing {
            Some(old) => format!("stored:replacing:{}", old.as_str()),
            None => "stored:fresh".to_owned(),
        };
        Ok(DraftReceipt::stored(ProviderKey::new(placed).unwrap()).with_keywords(keywords.clone()))
    }

    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        assert_eq!(draft.as_str(), "imap:v1:u7@Drafts");
        Ok(())
    }
}

impl crate::CalendarSync for FakeDrafts {}

#[tokio::test]
async fn a_boxed_adapter_reaches_its_own_draft_verbs() {
    let boxed: Box<dyn Provider> = Box::new(FakeDrafts);
    let key = ProviderKey::new("imap:v1:u7@Drafts").unwrap();

    assert!(boxed.connection_info().capabilities.mail_drafts());
    let keywords = BTreeSet::from([Keyword::new("project-x").unwrap()]);
    let fresh = boxed
        .put_draft(&account(), &draft(), None, &keywords)
        .await
        .unwrap();
    assert_eq!(fresh.key.as_str(), "stored:fresh");
    assert_eq!(fresh.keywords, keywords, "the keywords reach the adapter");
    assert_eq!(
        boxed
            .put_draft(&account(), &draft(), Some(&key), &none())
            .await
            .unwrap()
            .key
            .as_str(),
        "stored:replacing:imap:v1:u7@Drafts"
    );
    boxed.delete_draft(&account(), &key).await.unwrap();
}
