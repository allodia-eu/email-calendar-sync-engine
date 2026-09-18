//! Gated live integration: storing and replacing a draft against a real Microsoft Graph
//! account.
//!
//! What only a real server can answer: that `POST /me/messages` accepts the same
//! base64 MIME `/sendMail` takes and files the result as a draft, that the response
//! carries the id the key is built from, and that deleting an absent message answers
//! `404` rather than something the idempotent path would mistake for a failure. The
//! fixture-replay fake serves canned bytes whatever it is sent, so none of that is
//! testable offline.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set, so `cargo test --workspace` stays green:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_drafts -- --nocapture
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, SystemKeyword},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_graph::{GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The throwaway test account's own address; nothing here is ever sent.
const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A provider bound to `folder` (Graph accepts a well-known alias in the URL).
fn provider(token: String, folder: &str) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from(folder).unwrap())
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

/// The subjects of every message in Drafts carrying `message_id`.
async fn drafts_carrying(provider: &GraphProvider, message_id: &str) -> Vec<String> {
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
async fn live_graph_replacing_a_draft_leaves_exactly_one() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_replacing_a_draft: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let drafts = provider(token, "drafts");
    assert!(drafts.connection_info().capabilities.mail_drafts());

    // A fresh `Message-ID` per run: the assertions count copies carrying it, so a run
    // that aborted before its cleanup would otherwise be counted by every later run.
    let message_id = &format!(
        "live-graph-draft-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    let first = drafts
        .put_draft(&account(), &draft(message_id, "First version"), None)
        .await
        .expect("first save");
    println!("graph draft key: {}", first.as_str());
    assert_eq!(
        drafts_carrying(&drafts, message_id).await,
        vec!["First version".to_owned()]
    );

    let second = drafts
        .put_draft(
            &account(),
            &draft(message_id, "Second version"),
            Some(&first),
        )
        .await
        .expect("second save");
    assert_ne!(first, second, "Graph has no MIME update, so the key moves");
    assert_eq!(
        drafts_carrying(&drafts, message_id).await,
        vec!["Second version".to_owned()],
        "replacing left a duplicate behind"
    );

    drafts
        .delete_draft(&account(), &second)
        .await
        .expect("discard");
    assert!(drafts_carrying(&drafts, message_id).await.is_empty());

    // Retried, because the op behind it is retryable: the second call names a message
    // the first one deleted, and has to settle rather than park. This is the assertion
    // that pins Graph's `404`, which no offline fixture could have established.
    drafts
        .delete_draft(&account(), &second)
        .await
        .expect("a second discard is done, not an error");
}

#[tokio::test]
async fn live_graph_a_saved_draft_is_filed_as_a_draft() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_saved_draft_filing: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let drafts = provider(token, "drafts");

    let message_id = &format!(
        "live-graph-draft-role-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let key = drafts
        .put_draft(&account(), &draft(message_id, "Filed as a draft"), None)
        .await
        .expect("save");

    // `POST /me/messages` files into Drafts by Graph's own default, with the draft
    // keyword set: nothing in the request names a folder, so this is the server's
    // behaviour and not ours.
    let folders = drafts
        .sync_mailboxes(&account(), None)
        .await
        .expect("folders");
    let SyncUpdate::Snapshot { objects, .. } = folders.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let drafts_folder = objects
        .iter()
        .find(|m| m.role.as_ref() == Some(&MailboxRole::Drafts))
        .expect("the account has a Drafts folder");

    let emails = drafts.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let saved = objects
        .iter()
        .find(|m| m.id.key() == &key)
        .expect("the saved draft syncs back");
    assert!(saved.mailboxes.contains(&drafts_folder.id));
    assert!(
        saved.has_system_keyword(SystemKeyword::Draft),
        "Graph marks it a draft: {:?}",
        saved.keywords
    );

    drafts
        .delete_draft(&account(), &key)
        .await
        .expect("cleanup");
}
