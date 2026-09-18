//! Gated live integration: storing and replacing a draft against a real Gmail account.
//!
//! Gmail is the one transport here with a draft object of its own, and what only the
//! real API can answer is whether that object behaves as documented: that
//! `drafts.update` **replaces** a draft's content while keeping the draft's id, that the
//! message underneath it is a new one, and what `drafts.delete` answers for a draft that
//! is already gone. The fixture-replay fake serves canned bytes whatever it is sent, so
//! none of that is testable offline.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set, so `cargo test --workspace` stays green:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_drafts -- --nocapture
//! ```

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The test account's own address; nothing here is ever sent.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

fn draft(message_id: &str, subject: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(SELF_ADDRESS),
        vec![EmailAddress::new(SELF_ADDRESS)],
        subject,
        "Saved as a draft by the live test (never sent).",
    )
}

/// A fresh `Message-ID` per run: the assertions count copies carrying it, so a run that
/// aborted before its cleanup would otherwise be counted by every later run.
fn unique_message_id(prefix: &str) -> String {
    format!(
        "live-gmail-{prefix}-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    )
}

/// The subjects of every message in the account carrying `message_id`.
async fn messages_carrying(provider: &GmailProvider, message_id: &str) -> Vec<String> {
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
async fn live_gmail_replacing_a_draft_keeps_its_draft_id() {
    let Some(token) = token() else {
        eprintln!("skipping live_gmail_replacing_a_draft: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let p = provider(token);
    assert!(p.connection_info().capabilities.mail_drafts());

    let message_id = &unique_message_id("draft");

    let first = p
        .put_draft(&account(), &draft(message_id, "First version"), None)
        .await
        .expect("first save");
    println!("gmail draft id: {}", first.as_str());
    assert_eq!(
        messages_carrying(&p, message_id).await,
        vec!["First version".to_owned()]
    );

    let second = p
        .put_draft(
            &account(),
            &draft(message_id, "Second version"),
            Some(&first),
        )
        .await
        .expect("second save");

    // The one adapter here whose key survives a re-save: `drafts.update` replaces the
    // content of *this* draft, so there is nothing to clean up and no window with two.
    assert_eq!(first, second, "a Gmail draft keeps its id across an update");
    assert_eq!(
        messages_carrying(&p, message_id).await,
        vec!["Second version".to_owned()],
        "the update left the old version behind"
    );

    p.delete_draft(&account(), &second).await.expect("discard");
    assert!(messages_carrying(&p, message_id).await.is_empty());

    // Retried, because the op behind it is retryable: the second call names a draft the
    // first one deleted, and has to settle rather than park.
    p.delete_draft(&account(), &second)
        .await
        .expect("a second discard is done, not an error");
}

#[tokio::test]
async fn live_gmail_a_saved_draft_is_labelled_a_draft() {
    let Some(token) = token() else {
        eprintln!("skipping live_gmail_saved_draft_labels: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let p = provider(token);
    let message_id = &unique_message_id("draft-label");

    let key = p
        .put_draft(&account(), &draft(message_id, "Labelled a draft"), None)
        .await
        .expect("save");

    // `drafts.create` puts the message under the DRAFT label by Gmail's own behaviour:
    // nothing in the request names a label, so this is the server's doing, not ours.
    let labels = p.sync_mailboxes(&account(), None).await.expect("labels");
    let SyncUpdate::Snapshot { objects, .. } = labels.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let drafts_label = objects
        .iter()
        .find(|m| m.role.as_ref() == Some(&MailboxRole::Drafts))
        .expect("the account has a DRAFT label");

    let emails = p.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let saved = objects
        .iter()
        .find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .expect("the saved draft syncs back");
    assert!(saved.mailboxes.contains(&drafts_label.id));
    assert!(saved.has_system_keyword(SystemKeyword::Draft));

    // The key is the *draft* id, and the synced message carries a different one: the two
    // ids are not interchangeable, which is why only these two verbs ever see this key.
    assert_ne!(
        saved.id.key().as_str(),
        key.as_str(),
        "a Gmail draft id is not its message id"
    );

    p.delete_draft(&account(), &key).await.expect("cleanup");
}
