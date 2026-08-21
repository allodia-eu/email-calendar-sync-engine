//! Gated live checks that a Gmail snapshot and a Gmail delta agree about a message in
//! Junk or Trash.
//!
//! `messages.list` omits `SPAM` and `TRASH` unless asked; `history.list` takes no such
//! flag and reports their label changes regardless. So the two passes can disagree, and
//! the disagreement is not symmetric: a snapshot tombstones every key absent from its
//! present set, so the snapshot wins by *deleting* what the delta had just filed
//! correctly. Which one the store believes depends on whether the last pass happened to
//! be a snapshot — history aging out is enough to turn one into the other.
//!
//! That is what these tests assert, and it is why each one drives **both** passes over
//! the same message rather than reading the snapshot alone.
//!
//! Each test sends its own self-addressed throwaway and permanently deletes it
//! afterwards, so nothing here depends on what the shared account happens to hold.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_spam_trash -- --nocapture --test-threads=1
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::EmailAddress,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, ReportVerdict, ReportingProvider};
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The test account's own address — every probe is self-addressed, so nothing leaves the
/// mailbox.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client =
        GoogleClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GmailProvider::new(client)
}

/// Sends a self-addressed throwaway and returns its key.
async fn send_probe(provider: &GmailProvider, marker: &str) -> ProviderKey {
    let message_id = MessageIdHeader::new(format!("gmail-place-{marker}@example.test")).unwrap();
    let draft = Draft::new(
        message_id,
        EmailAddress::new(SELF_ADDRESS),
        vec![EmailAddress::new(SELF_ADDRESS)],
        format!("Live place probe {marker}"),
        "Place probe body.",
    );
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("send the probe")
        .email_key
}

/// A snapshot pass, and the cursor to take a delta from.
async fn snapshot(provider: &GmailProvider) -> (Vec<engine_core::mail::Message>, SyncState) {
    let scope = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    let cursor = scope.next_cursor.clone();
    let SyncUpdate::Snapshot { objects, .. } = scope.update else {
        panic!("a first pass is a snapshot");
    };
    (objects, cursor)
}

/// Where the **snapshot** says `key` is filed, or `None` when the snapshot does not carry
/// it at all — which is the failure this file exists to catch, because a key missing from
/// a snapshot's present set is tombstoned rather than merely unreported.
async fn snapshot_places(provider: &GmailProvider, key: &ProviderKey) -> Option<Vec<String>> {
    let (objects, _) = snapshot(provider).await;
    objects
        .iter()
        .find(|message| message.id.key() == key)
        .map(|message| places(message.mailboxes.iter()))
}

/// Where the **delta** from `cursor` says `key` is filed, or `None` when the delta did not
/// mention it. Reads both halves of a page: a filing change arrives as a patch, but Gmail
/// re-reports the whole message when the page could not answer it alone.
async fn delta_places(
    provider: &GmailProvider,
    cursor: &SyncState,
    key: &ProviderKey,
) -> Option<Vec<String>> {
    let scope = provider
        .sync_email(&account(), Some(cursor))
        .await
        .expect("delta");
    let SyncUpdate::Delta {
        changed, patched, ..
    } = scope.update
    else {
        panic!("a pass from a cursor is a delta");
    };
    // `changed` wins over `patched` for one key, as the delta contract says — and Gmail's
    // history really can report both for a single id in one page.
    changed
        .iter()
        .find(|message| message.id.key() == key)
        .map(|message| places(message.mailboxes.iter()))
        .or_else(|| {
            patched
                .iter()
                .find(|change| &change.key == key)
                .and_then(|change| change.state.mailboxes.as_ref())
                .map(|boxes| places(boxes.iter()))
        })
}

/// Mailbox ids as owned strings, for an assertion message that names them.
fn places<'a>(ids: impl Iterator<Item = &'a MailboxId>) -> Vec<String> {
    ids.map(|id| id.as_str().to_owned()).collect()
}

async fn cleanup(provider: &GmailProvider, key: ProviderKey) {
    if let Err(err) = provider.edit_mail(&account(), &MailEdit::delete(key)).await {
        eprintln!("cleanup delete gave up (leaving throwaway): {err}");
    }
}

#[tokio::test]
async fn a_trashed_message_is_in_both_the_delta_and_the_snapshot() {
    let Some(token) = token() else {
        eprintln!("skipping live Gmail trash-visibility test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let key = send_probe(&provider, &format!("t{}", std::process::id())).await;

    let (_, cursor) = snapshot(&provider).await;
    provider
        .edit_mail(
            &account(),
            &MailEdit::move_to(key.clone(), MailboxId::try_from("TRASH").unwrap()),
        )
        .await
        .expect("trash the probe");

    let delta = delta_places(&provider, &cursor, &key)
        .await
        .expect("the delta reports the trashing");
    assert!(
        delta.contains(&"TRASH".to_owned()),
        "the delta files it in Trash: {delta:?}"
    );

    let snap = snapshot_places(&provider, &key)
        .await
        .expect("the snapshot still carries a trashed message — absent means it is tombstoned");
    assert!(
        snap.contains(&"TRASH".to_owned()),
        "and agrees about where: {snap:?}"
    );

    cleanup(&provider, key).await;
}

#[tokio::test]
async fn a_junk_reported_message_is_in_both_the_delta_and_the_snapshot() {
    let Some(token) = token() else {
        eprintln!("skipping live Gmail junk-visibility test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let key = send_probe(&provider, &format!("s{}", std::process::id())).await;

    let (_, cursor) = snapshot(&provider).await;
    provider
        .report_message(
            &account(),
            &MessageReport::new(
                key.clone(),
                ReportVerdict::Junk,
                MailboxId::try_from("SPAM").unwrap(),
            ),
        )
        .await
        .expect("report junk");

    let delta = delta_places(&provider, &cursor, &key)
        .await
        .expect("the delta reports the report");
    assert!(
        delta.contains(&"SPAM".to_owned()),
        "the delta files it in Junk: {delta:?}"
    );

    // Without this the not-junk direction is unreachable from a synced row: the message
    // the user would have to press "not junk" on is not in the account the engine shows.
    let snap = snapshot_places(&provider, &key)
        .await
        .expect("the snapshot still carries a junk-reported message");
    assert!(
        snap.contains(&"SPAM".to_owned()),
        "and agrees about where: {snap:?}"
    );

    cleanup(&provider, key).await;
}
