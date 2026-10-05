//! Gated live integration: a JMAP send reaches **every** recipient, keeps its threading
//! headers, and shows no recipient the Bcc list.
//!
//! The offline suite asserts the request; only a real server says who a submission
//! delivered to. Each assertion reads the recipient's own mailbox, so a send that listed a
//! recipient nowhere the server acts on fails here rather than passing.
//!
//! Runs between the two scratch accounts, which no suite counts: `carol@` sends, `bob@`
//! receives as Cc in one message and as Bcc in the other, and `carol@` addresses herself in
//! To so a To recipient's copy can be read for a leaked Bcc header. Every `Message-ID` is
//! unique to the run, and what the test delivered it deletes. Skips with no
//! `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, ScratchAccount};

fn account() -> AccountId {
    AccountId::try_from("live-submit-recipients").unwrap()
}

/// A `Message-ID` no other run shares.
fn unique_id(label: &str) -> MessageIdHeader {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    MessageIdHeader::new(format!("live-jmap-{label}-{nanos}@test.local")).unwrap()
}

async fn connect(harness: &Harness, who: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&who.address, &who.password),
    ))
    .await
    .expect("connect")
}

async fn inbox(provider: &JmapProvider) -> MailboxId {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .unwrap()
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Inbox))
        .expect("an Inbox")
        .id
}

/// The copy of `message_id` delivered to `provider`'s Inbox, waiting for delivery.
///
/// `None` once the wait runs out: delivery is asynchronous, and a recipient the submission
/// never named is never delivered to at all.
async fn delivered(provider: &JmapProvider, message_id: &MessageIdHeader) -> Option<Message> {
    let inbox = inbox(provider).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let SyncUpdate::Snapshot { objects, .. } =
            provider.sync_email(&account(), None).await.unwrap().update
        else {
            panic!("a cursorless sync is a snapshot");
        };
        let found = objects.into_iter().find(|message| {
            message.mailboxes.contains(&inbox) && message.envelope.message_id.contains(message_id)
        });
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The raw header block of `message`, as the recipient's server stored it.
async fn raw_headers(provider: &JmapProvider, message: &Message) -> String {
    let raw = provider
        .fetch_message_source(&account(), message)
        .await
        .expect("download raw source");
    let text = String::from_utf8_lossy(raw.as_bytes()).into_owned();
    text.split("\r\n\r\n").next().unwrap_or_default().to_owned()
}

async fn remove(provider: &JmapProvider, message: &Message) {
    provider
        .edit_mail(&account(), &MailEdit::delete(message.id.key().clone()))
        .await
        .expect("cleanup delete");
}

fn has_header(headers: &str, name: &str) -> bool {
    headers
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with(&format!("{name}:")))
}

#[tokio::test]
async fn a_cc_recipient_receives_the_reply_threaded() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_cc_recipient_receives_the_reply_threaded: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let [bob_auth, carol_auth] = harness.scratch.clone();
    let carol = connect(&harness, &carol_auth).await;
    let bob = connect(&harness, &bob_auth).await;

    let message_id = unique_id("cc");
    let parent = unique_id("cc-parent");
    let root = unique_id("cc-root");
    let draft = Draft::new(
        message_id.clone(),
        EmailAddress::new(&carol_auth.address),
        vec![EmailAddress::new(&carol_auth.address)],
        "Re: JMAP Cc probe",
        "Sent to Carol, with Bob in Cc.",
    )
    .with_cc(vec![EmailAddress::new(&bob_auth.address)])
    .in_reply_to(parent.clone(), vec![root.clone(), parent.clone()]);
    carol
        .submit_email(
            &account(),
            &draft,
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("submit");

    let received = delivered(&bob, &message_id)
        .await
        .expect("the Cc recipient receives the message");
    assert_eq!(received.envelope.in_reply_to, vec![parent.clone()]);
    assert_eq!(received.envelope.references, vec![root, parent]);
    assert!(
        received
            .envelope
            .cc
            .iter()
            .any(|cc| cc.email.eq_ignore_ascii_case(&bob_auth.address)),
        "the Cc header names Bob: {:?}",
        received.envelope.cc
    );

    remove(&bob, &received).await;
    if let Some(own) = delivered(&carol, &message_id).await {
        remove(&carol, &own).await;
    }
}

#[tokio::test]
async fn a_bcc_recipient_receives_the_message_and_no_recipient_sees_the_bcc() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_bcc_recipient_receives_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let [bob_auth, carol_auth] = harness.scratch.clone();
    let carol = connect(&harness, &carol_auth).await;
    let bob = connect(&harness, &bob_auth).await;

    let message_id = unique_id("bcc");
    let draft = Draft::new(
        message_id.clone(),
        EmailAddress::new(&carol_auth.address),
        vec![EmailAddress::new(&carol_auth.address)],
        "JMAP Bcc probe",
        "Sent to Carol, with Bob in Bcc.",
    )
    .with_bcc(vec![EmailAddress::new(&bob_auth.address)]);
    let receipt = carol
        .submit_email(
            &account(),
            &draft,
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("submit");

    let hidden = delivered(&bob, &message_id)
        .await
        .expect("the Bcc recipient receives the message");
    assert!(
        !has_header(&raw_headers(&bob, &hidden).await, "bcc"),
        "the Bcc recipient's copy carries no Bcc header"
    );
    let visible = delivered(&carol, &message_id)
        .await
        .expect("the To recipient receives the message");
    assert!(
        !has_header(&raw_headers(&carol, &visible).await, "bcc"),
        "the To recipient's copy carries no Bcc header"
    );

    // The sender's own copy keeps the Bcc list, as every other transport's filed copy does.
    let SyncUpdate::Snapshot { objects, .. } =
        carol.sync_email(&account(), None).await.unwrap().update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    let sent = objects
        .iter()
        .find(|message| message.id.key() == &receipt.email_key)
        .expect("the Sent copy");
    assert!(
        sent.envelope
            .bcc
            .iter()
            .any(|bcc| bcc.email.eq_ignore_ascii_case(&bob_auth.address)),
        "the Sent copy records the Bcc: {:?}",
        sent.envelope.bcc
    );

    remove(&bob, &hidden).await;
    remove(&carol, &visible).await;
    remove(&carol, sent).await;
}
