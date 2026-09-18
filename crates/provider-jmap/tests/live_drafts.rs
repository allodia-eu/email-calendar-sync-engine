//! Gated live integration: storing and replacing a draft over JMAP, against the
//! Stalwart harness.
//!
//! What only a real server can answer: that a single `Email/set` carrying both a
//! `create` and a `destroy` is processed in RFC 8620 §5.3's order, so the replacement
//! exists before the copy it supersedes goes and the account never holds two. The
//! offline fake serves canned bytes whatever it is sent, so that ordering is exactly
//! the kind of claim it cannot test.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so `cargo test --workspace` stays green offline.

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live-drafts").unwrap()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect")
}

fn draft(message_id: &str, subject: &str, from: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(from),
        vec![EmailAddress::new("bob@test.local")],
        subject,
        "Saved as a draft by the live test (never submitted).",
    )
}

/// The account's drafts, as the server holds them: every message carrying
/// `message_id`, with the subject each one records.
async fn drafts_carrying(provider: &JmapProvider, message_id: &str) -> Vec<String> {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .iter()
        .filter(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .map(|m| m.envelope.subject.clone().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn live_jmap_replacing_a_draft_leaves_exactly_one() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_replacing_a_draft: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&harness).await;

    // The server says it can keep a draft, which is what a host reads before offering
    // the action.
    assert!(provider.connection_info().capabilities.mail_drafts());

    // A fresh `Message-ID` per run: the assertions count copies carrying it, so a run
    // that aborted before its cleanup would otherwise be counted by every later run.
    let message_id = &format!(
        "live-jmap-draft-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    let first = provider
        .put_draft(
            &account(),
            &draft(message_id, "First version", &harness.account),
            None,
        )
        .await
        .expect("first save");
    assert_eq!(
        drafts_carrying(&provider, message_id).await,
        vec!["First version".to_owned()]
    );

    let second = provider
        .put_draft(
            &account(),
            &draft(message_id, "Second version", &harness.account),
            Some(&first),
        )
        .await
        .expect("second save");

    // One `Email/set` carried both halves, and the server applied them in the order the
    // spec fixes: the account holds the new version alone, never two and never none.
    assert_eq!(
        drafts_carrying(&provider, message_id).await,
        vec!["Second version".to_owned()],
        "replacing left a duplicate behind"
    );
    assert_ne!(first, second, "a replaced JMAP email is a new object");

    provider
        .delete_draft(&account(), &second)
        .await
        .expect("discard");
    assert!(drafts_carrying(&provider, message_id).await.is_empty());

    // Retried, because the op behind it is retryable: the second call names a message
    // the first one destroyed, and has to settle rather than park.
    provider
        .delete_draft(&account(), &second)
        .await
        .expect("a second discard is done, not an error");
}

#[tokio::test]
async fn live_jmap_a_saved_draft_lands_in_the_drafts_mailbox() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_saved_draft_mailbox: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&harness).await;

    let message_id = &format!(
        "live-jmap-draft-role-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let key = provider
        .put_draft(
            &account(),
            &draft(message_id, "Filed by role", &harness.account),
            None,
        )
        .await
        .expect("save");

    // Filed by the mailbox carrying the Drafts *role*, not by a folder named "Drafts":
    // the account's own name for it is the server's business.
    let mailboxes = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("mailboxes");
    let SyncUpdate::Snapshot { objects, .. } = mailboxes.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let drafts = objects
        .iter()
        .find(|m| m.role.as_ref() == Some(&MailboxRole::Drafts))
        .expect("the harness account has a Drafts mailbox");

    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let saved = objects
        .iter()
        .find(|m| m.id.key() == &key)
        .expect("the saved draft syncs back");
    assert!(
        saved.mailboxes.contains(&drafts.id),
        "a saved draft belongs to the Drafts mailbox"
    );

    provider
        .delete_draft(&account(), &key)
        .await
        .expect("cleanup");
}
