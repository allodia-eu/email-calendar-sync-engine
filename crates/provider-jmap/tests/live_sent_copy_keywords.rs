//! Gated live integration: a send asks for keywords on its filed Sent copy, the copy carries
//! them, and the recipient's copy does not.
//!
//! The keywords are set by an `Email/set` of their own after the submission; only a real
//! server says it accepts an update addressed to the email the same request created, so this
//! reads the copy back rather than trusting the answer. Runs between the two scratch
//! accounts: `carol@` sends, `bob@` receives. Every `Message-ID` is unique to the run, and
//! what the test delivered it deletes. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, Keyword, MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, ScratchAccount};

fn account() -> AccountId {
    AccountId::try_from("live-sent-copy-keywords").unwrap()
}

async fn connect(harness: &Harness, who: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&who.address, &who.password),
    ))
    .await
    .expect("connect")
}

/// The copy of `message_id` in `provider`'s folder carrying `role`, waiting for it to land.
async fn copy_in(
    provider: &JmapProvider,
    role: MailboxRole,
    message_id: &MessageIdHeader,
) -> Option<Message> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .unwrap()
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    let folder = objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(role.clone()))
        .expect("the folder exists")
        .id;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let SyncUpdate::Snapshot { objects, .. } =
            provider.sync_email(&account(), None).await.unwrap().update
        else {
            panic!("a cursorless sync is a snapshot");
        };
        let found = objects.into_iter().find(|message| {
            message.mailboxes.contains(&folder) && message.envelope.message_id.contains(message_id)
        });
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn remove(provider: &JmapProvider, message: &Message) {
    provider
        .edit_mail(&account(), &MailEdit::delete(message.id.key().clone()))
        .await
        .expect("cleanup delete");
}

#[tokio::test]
async fn the_filed_copy_carries_the_keywords_and_the_delivered_one_does_not() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping the_filed_copy_carries_the_keywords_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let [bob_auth, carol_auth] = harness.scratch.clone();
    let carol = connect(&harness, &carol_auth).await;
    let bob = connect(&harness, &bob_auth).await;
    assert!(carol.connection_info().capabilities.sent_copy_keywords());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id =
        MessageIdHeader::new(format!("live-jmap-sent-keywords-{nanos}@test.local")).unwrap();
    let keyword = Keyword::new("project-x").unwrap();
    let draft = Draft::new(
        message_id.clone(),
        EmailAddress::new(&carol_auth.address),
        vec![EmailAddress::new(&bob_auth.address)],
        "Sent copy keywords over JMAP",
        "The sender's copy of this message carries a keyword.",
    )
    .with_sent_copy_keyword(keyword.clone());

    let receipt = carol
        .submit_email(
            &account(),
            &draft,
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("submit");
    assert!(
        receipt.sent_copy_keywords.contains(&keyword),
        "the receipt says the copy kept the keyword: {:?}",
        receipt.sent_copy_keywords
    );

    let filed = copy_in(&carol, MailboxRole::Sent, &message_id)
        .await
        .expect("the copy is filed in Sent");
    let delivered = copy_in(&bob, MailboxRole::Inbox, &message_id)
        .await
        .expect("the recipient receives the message");
    let (filed_has, delivered_has) = (
        filed.keywords.contains(&keyword),
        delivered.keywords.contains(&keyword),
    );
    let still_a_draft = filed.keywords.iter().any(|k| k.as_str() == "$draft");
    remove(&carol, &filed).await;
    remove(&bob, &delivered).await;

    assert!(
        filed_has,
        "the filed copy carries the keyword: {:?}",
        filed.keywords
    );
    assert!(!still_a_draft, "the filed copy is no longer a draft");
    assert!(
        !delivered_has,
        "the recipient's copy carries none: {:?}",
        delivered.keywords
    );
}
