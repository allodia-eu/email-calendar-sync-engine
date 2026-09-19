//! Gated live integration: storing and replacing a draft against a real Microsoft Graph
//! account.
//!
//! What only a real server can answer: that `POST /me/messages` accepts the same
//! base64 MIME `/sendMail` takes and files the result as a draft, that `PATCH` really
//! does rewrite a draft's subject, body and recipients while keeping its id and its
//! `internetMessageId`, that it leaves the attachment collection alone (which is why an
//! attachment change falls back to a replacement), and what a retried delete answers.
//! The fixture-replay fake serves canned bytes whatever it is sent, so none of that is
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

/// The body of the stored message `key`, as the account's own sync fetched it.
async fn stored_body(provider: &GraphProvider, key: &engine_core::ids::ProviderKey) -> String {
    let source = provider
        .fetch_message_source(&account(), &stored_message(provider, key).await)
        .await
        .expect("fetch the stored draft's source");
    String::from_utf8_lossy(source.as_bytes()).into_owned()
}

/// The synced `Message` for `key`, which a source fetch needs to address it.
async fn stored_message(
    provider: &GraphProvider,
    key: &engine_core::ids::ProviderKey,
) -> engine_core::mail::Message {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.id.key() == key)
        .expect("the saved draft syncs back")
}

/// Whether the stored message `key` carries an attachment, as the account's own sync
/// reports it.
///
/// `Message::has_attachment` rather than `Message::attachments`: an envelope sync does
/// not fetch the attachment collection (that is the body-fetch path's job), so the list
/// is empty here whatever the server holds, while the flag is normalized from Graph's
/// own `hasAttachments`.
async fn has_attachment(provider: &GraphProvider, key: &engine_core::ids::ProviderKey) -> bool {
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

    // The whole point of the in-place rewrite: a text edit keeps the id, so the draft
    // does not jump in the folder and the caller's key stays good. This is the shape a
    // repeated save takes.
    assert_eq!(
        first, second,
        "a text-only re-save rewrites the stored draft rather than replacing it"
    );
    assert_eq!(
        drafts_carrying(&drafts, message_id).await,
        vec!["Second version".to_owned()],
        "the rewrite left the old version behind"
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

#[tokio::test]
async fn live_graph_a_rewritten_draft_keeps_its_message_id() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_rewritten_message_id: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let drafts = provider(token, "drafts");

    let message_id = &format!(
        "live-graph-draft-header-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    let first = drafts
        .put_draft(&account(), &draft(message_id, "Header v1"), None)
        .await
        .expect("first save");
    let second = drafts
        .put_draft(&account(), &draft(message_id, "Header v2"), Some(&first))
        .await
        .expect("rewrite");
    assert_eq!(first, second);

    // Unlike Gmail, Graph preserves the `Message-ID` we wrote — through the MIME create
    // *and* through the rewrite. So on this transport the header stays a usable join
    // between a host's own copy and the stored draft, which is worth pinning because the
    // two adapters differ on exactly this point.
    let emails = drafts.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let saved = objects
        .iter()
        .find(|m| m.id.key() == &second)
        .expect("the rewritten draft syncs back");
    assert!(
        saved
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == message_id),
        "Graph kept our Message-ID across the rewrite: {:?}",
        saved.envelope.message_id
    );
    assert_eq!(saved.envelope.subject.as_deref(), Some("Header v2"));

    drafts
        .delete_draft(&account(), &second)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn live_graph_an_attachment_change_keeps_the_draft() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_attachment_change: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let drafts = provider(token, "drafts");

    let message_id = &format!(
        "live-graph-draft-attach-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    let first = drafts
        .put_draft(&account(), &draft(message_id, "Attach v1"), None)
        .await
        .expect("first save");

    // Gain an attachment.
    let mut with_file = draft(message_id, "Attach v2");
    with_file.attachments = vec![engine_provider::DraftAttachment::attachment(
        "note.txt",
        "text/plain",
        b"hello".to_vec(),
    )];
    let second = drafts
        .put_draft(&account(), &with_file, Some(&first))
        .await
        .expect("save with an attachment");

    // An attachment is a resource of its own, so gaining one does not cost a new
    // message: the draft keeps its id, and a host's stored key stays good.
    assert_eq!(first, second, "gaining an attachment kept the draft");
    assert!(
        has_attachment(&drafts, &second).await,
        "the uploaded attachment is not on the stored draft"
    );

    // And lose it again.
    let third = drafts
        .put_draft(&account(), &draft(message_id, "Attach v3"), Some(&second))
        .await
        .expect("save without the attachment");
    assert_eq!(first, third, "losing an attachment kept the draft");
    assert!(
        !has_attachment(&drafts, &third).await,
        "the attachment the draft no longer carries is still stored"
    );
    assert_eq!(
        drafts_carrying(&drafts, message_id).await,
        vec!["Attach v3".to_owned()],
        "three saves, one draft"
    );

    drafts
        .delete_draft(&account(), &third)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn live_graph_an_html_draft_is_stored_as_html_by_both_paths() {
    let Some(token) = token() else {
        eprintln!("skipping live_graph_html_draft: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let drafts = provider(token, "drafts");

    let message_id = &format!(
        "live-graph-draft-html-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let rich =
        |subject: &str, html: &str| draft(message_id, subject).with_html_body(html.to_owned());

    // The create goes out as base64 MIME under an HTTP `Content-Type: text/plain`, which
    // describes the *request* and not the message: the HTML rides inside the MIME as a
    // `multipart/alternative`. This asserts Graph parsed it that way rather than storing
    // the whole payload as text, which is the thing the request header looks like it says.
    let created = drafts
        .put_draft(
            &account(),
            &rich("HTML v1", "<p><strong>one</strong></p>"),
            None,
        )
        .await
        .expect("first save");
    let body = stored_body(&drafts, &created).await;
    assert!(
        body.contains("<strong>one</strong>"),
        "the create lost the HTML: {body}"
    );

    // And the rewrite, which states the content type outright rather than through MIME.
    let rewritten = drafts
        .put_draft(
            &account(),
            &rich("HTML v2", "<p><em>two</em></p>"),
            Some(&created),
        )
        .await
        .expect("rewrite");
    let body = stored_body(&drafts, &rewritten).await;
    assert!(
        body.contains("<em>two</em>"),
        "the rewrite lost the HTML: {body}"
    );

    drafts
        .delete_draft(&account(), &rewritten)
        .await
        .expect("cleanup");
}
