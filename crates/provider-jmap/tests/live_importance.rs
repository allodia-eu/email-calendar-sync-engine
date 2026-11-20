//! Gated live integration: a JMAP message's importance reaches its recipient and survives as a
//! draft, against the Stalwart harness.
//!
//! JMAP has no importance property, so the adapter names the `Importance` and `X-Priority`
//! headers as `header:` properties on `Email/set` and `Email/get`. Only a real server says whether
//! it accepts them on a create and answers them on a read; the offline fake answers canned bytes
//! whatever it is sent. The normal draft is the control: it is created without either header, and
//! reads back normal.
//!
//! Runs between the two scratch accounts, which no suite counts: `carol@` sends and `bob@`
//! receives. Every `Message-ID` is unique to the run, and what the test stored it deletes. Skips
//! with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, Importance, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, ScratchAccount};

fn account() -> AccountId {
    AccountId::try_from("live-importance").unwrap()
}

fn unique_id(label: &str) -> MessageIdHeader {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    MessageIdHeader::new(format!("live-jmap-importance-{label}-{nanos}@test.local")).unwrap()
}

async fn connect(harness: &Harness, who: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&who.address, &who.password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect")
}

/// The message carrying `message_id` in `provider`'s account, waiting for delivery.
async fn find(provider: &JmapProvider, message_id: &MessageIdHeader) -> Option<Message> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let SyncUpdate::Snapshot { objects, .. } =
            provider.sync_email(&account(), None).await.unwrap().update
        else {
            panic!("a cursorless sync is a snapshot");
        };
        let found = objects
            .into_iter()
            .find(|message| message.envelope.message_id.contains(message_id));
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

fn draft(from: &str, to: &str, message_id: &MessageIdHeader, importance: Importance) -> Draft {
    Draft::new(
        message_id.clone(),
        EmailAddress::new(from),
        vec![EmailAddress::new(to)],
        "Importance probe",
        "Sent by the live importance test.",
    )
    .with_importance(importance)
}

#[tokio::test]
async fn a_high_importance_send_reaches_the_recipient_as_high() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping a_high_importance_send_reaches_the_recipient: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("ready");
    let [bob_auth, carol_auth] = harness.scratch.clone();
    let carol = connect(&harness, &carol_auth).await;
    let bob = connect(&harness, &bob_auth).await;

    let message_id = unique_id("send");
    carol
        .submit_email(
            &account(),
            &draft(
                &carol_auth.address,
                &bob_auth.address,
                &message_id,
                Importance::High,
            ),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("submit");

    let received = find(&bob, &message_id)
        .await
        .expect("the recipient receives the message");
    assert_eq!(received.envelope.importance, Importance::High);
    // The header itself reached the recipient, not only a property of the sender's server.
    let raw = bob
        .fetch_message_source(&account(), &received)
        .await
        .expect("download raw source");
    let text = String::from_utf8_lossy(raw.as_bytes()).to_ascii_lowercase();
    let headers = text.split("\r\n\r\n").next().unwrap_or_default();
    assert!(headers.contains("\r\nimportance: high"), "{headers}");
    assert!(headers.contains("\r\nx-priority: 1"), "{headers}");

    remove(&bob, &received).await;
    if let Some(sent) = find(&carol, &message_id).await {
        remove(&carol, &sent).await;
    }
}

#[tokio::test]
async fn a_drafts_importance_reads_back() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_drafts_importance_reads_back: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("ready");
    let [_, carol_auth] = harness.scratch.clone();
    let carol = connect(&harness, &carol_auth).await;

    for importance in [Importance::Low, Importance::High, Importance::Normal] {
        let message_id = unique_id(importance.as_str());
        let key = carol
            .put_draft(
                &account(),
                &draft(
                    &carol_auth.address,
                    "bob@test.local",
                    &message_id,
                    importance,
                ),
                None,
                &std::collections::BTreeSet::new(),
            )
            .await
            .map(|stored| stored.key)
            .expect("save");
        let stored = find(&carol, &message_id).await.expect("the saved draft");
        assert_eq!(stored.envelope.importance, importance);
        carol.delete_draft(&account(), &key).await.expect("discard");
    }
}
