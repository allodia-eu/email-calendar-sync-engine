//! Gated live integration: storing and replacing a draft against a real Gmail account.
//!
//! Gmail is the one transport here with a draft object of its own, and what only the
//! real API can answer is whether that object behaves as documented: that
//! `drafts.update` **replaces** a draft's content while keeping the draft's id, that the
//! message underneath it is a new one, and what `drafts.delete` answers for a draft that
//! is already gone. The fixture-replay fake serves canned bytes whatever it is sent, so
//! none of that is testable offline.
//!
//! ⚠️ **Gmail rewrites the `Message-ID` on `drafts.create`**, exactly as it does on send
//! (`google.md`). So nothing here identifies a draft by the header it was written with:
//! a saved draft is found by its subject, and the rewrite itself is pinned below, since
//! a host that tried to correlate its local copy by `Message-ID` would match nothing.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set, so `cargo test --workspace` stays green:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_drafts -- --nocapture
//! ```

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message, SystemKeyword},
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

/// A value unique to this run, so a run that aborted before its cleanup cannot be
/// counted by a later one.
fn unique_run_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos()
}

/// One whole-account snapshot.
///
/// Called **once per phase and reused**, never once per assertion: a Gmail snapshot
/// fetches every message, and a handful of them in quick succession exhausts the
/// project's "Total Query Cost" quota (6000 units per minute per user) and fails the run
/// for a reason that has nothing to do with drafts.
async fn snapshot(provider: &GmailProvider) -> Vec<Message> {
    let emails = provider.sync_email(&account(), None).await.expect("sync");
    let SyncUpdate::Snapshot { objects, .. } = emails.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
}

/// Every message in `snapshot` whose subject is `subject`.
///
/// By subject rather than by `Message-ID`, which Gmail rewrote on the way in.
fn titled<'a>(snapshot: &'a [Message], subject: &str) -> Vec<&'a Message> {
    snapshot
        .iter()
        .filter(|m| m.envelope.subject.as_deref() == Some(subject))
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

    // Unique per run, so a run that aborted before its cleanup cannot be counted by a
    // later one. The subject carries the uniqueness because the `Message-ID` will not
    // survive the save.
    let run = unique_run_id();
    let first_subject = &format!("Live draft {run}: first version");
    let second_subject = &format!("Live draft {run}: second version");
    let message_id = &format!("live-gmail-draft-{run}@test.local");

    let first = p
        .put_draft(&account(), &draft(message_id, first_subject), None)
        .await
        .expect("first save");
    println!("gmail draft id: {}", first.as_str());
    let after_first = snapshot(&p).await;
    let saved = titled(&after_first, first_subject);
    assert_eq!(saved.len(), 1, "one save, one draft");

    // Pinned because it decides how a host correlates its own copy: Gmail replaced the
    // `Message-ID` we wrote with one of its own, so the header cannot be the join key
    // and the returned draft key has to be.
    assert!(
        !saved[0]
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == message_id),
        "Gmail is expected to rewrite the Message-ID on drafts.create; it kept ours: {:?}",
        saved[0].envelope.message_id
    );

    let second = p
        .put_draft(&account(), &draft(message_id, second_subject), Some(&first))
        .await
        .expect("second save");

    // The one adapter here whose key survives a re-save: `drafts.update` replaces the
    // content of *this* draft, so there is nothing to clean up and no window with two.
    assert_eq!(first, second, "a Gmail draft keeps its id across an update");

    // One snapshot answers both halves of what a replacement means.
    let after_update = snapshot(&p).await;
    assert_eq!(
        titled(&after_update, second_subject).len(),
        1,
        "the update did not store the new version"
    );
    assert!(
        titled(&after_update, first_subject).is_empty(),
        "the update left the old version behind"
    );

    p.delete_draft(&account(), &second).await.expect("discard");

    // Retried, because the op behind it is retryable: the second call names a draft the
    // first one deleted, and has to settle rather than park. Its `404` is also the
    // evidence the first delete landed, so no further snapshot is spent proving that.
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
    let run = unique_run_id();
    let subject = &format!("Live draft {run}: labelled");
    let message_id = &format!("live-gmail-draft-label-{run}@test.local");

    let key = p
        .put_draft(&account(), &draft(message_id, subject), None)
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
        .find(|m| m.envelope.subject.as_deref() == Some(subject.as_str()))
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
