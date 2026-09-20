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
use engine_store::{ManualClock, PendingOpState, StoreRead, WorkerId};
use engine_sync::put_draft_mail;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

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

/// Whether the stored message `key` carries an attachment, as the account's own sync
/// reports it.
///
/// `Message::has_attachment` rather than `Message::attachments`: the mail sync selects
/// `hasAttachment` and not the attachments collection, so the list is empty here whatever
/// the server holds.
async fn has_attachment(provider: &JmapProvider, key: &engine_core::ids::ProviderKey) -> bool {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .iter()
        .find(|m| m.id.key() == key)
        .expect("the saved draft syncs back")
        .has_attachment
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

#[tokio::test]
async fn live_jmap_a_resaved_attachment_is_not_sent_again() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_resaved_attachment: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&harness).await;

    let message_id = &format!(
        "live-jmap-draft-blob-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let attached = |subject: &str| {
        let mut draft = draft(message_id, subject, &harness.account);
        draft.attachments = vec![engine_provider::DraftAttachment::attachment(
            "note.txt",
            "text/plain",
            b"the same bytes both times".to_vec(),
        )];
        draft
    };

    let first = provider
        .put_draft(&account(), &attached("Blob v1"), None)
        .await
        .expect("first save");
    assert!(
        has_attachment(&provider, &first).await,
        "the attachment is on the stored draft"
    );

    let second = provider
        .put_draft(&account(), &attached("Blob v2"), Some(&first))
        .await
        .expect("second save");

    // An `Email` is immutable, so the replacement is a new object — but the bytes are
    // not re-sent: it references the blob the stored draft already carried. Saving a
    // draft with a large file attached therefore costs that file once, not once per save.
    // The replacement references the blob the stored draft carried rather than sending
    // the bytes again. A `blobId` the server would not accept fails the whole
    // `Email/set` with `blobNotFound`, so a save that succeeds with its attachment intact
    // is what proves the reference was good. How many uploads it cost is counted exactly
    // in the offline suite, which is the only place a request can be counted.
    assert_ne!(first, second, "a replaced JMAP email is a new object");
    assert!(
        has_attachment(&provider, &second).await,
        "the re-saved draft lost its attachment"
    );

    provider
        .delete_draft(&account(), &second)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn live_jmap_a_draft_saved_through_the_outbox_settles_and_is_on_the_server() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_outbox_draft: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = connect(&harness).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");

    let message_id = &format!(
        "live-jmap-outbox-draft-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    // The whole path a host takes, against a real server and a real store: durable op,
    // claim, provider call, settle. The offline half is driven by the fake, which is the
    // right tool for simulating an outage; what only a server can answer is that the op
    // settles against the key the provider actually minted.
    let saved = put_draft_mail(
        &provider,
        &store,
        &account(),
        WorkerId::new("live-outbox"),
        core::time::Duration::from_secs(30),
        &draft(message_id, "Through the outbox", &harness.account),
        None,
    )
    .await
    .expect("save through the outbox");

    assert_eq!(
        store.pending_op_state(saved.op).await.unwrap(),
        Some(PendingOpState::Succeeded),
        "the op did not settle"
    );
    assert_eq!(
        drafts_carrying(&provider, message_id).await,
        vec!["Through the outbox".to_owned()]
    );

    provider
        .delete_draft(&account(), &saved.key)
        .await
        .expect("cleanup");
}
