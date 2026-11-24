//! Gated live proof that a Gmail pass reaches a label held further back than the account, and
//! no other label, over the API and over IMAP.
//!
//! The account's window is put after today, so its own listing holds nothing, and Sent and one
//! label of the user's are held back to 2019. Three messages dated 2020 are planted with
//! `messages.insert` (nothing is sent): one in Sent, one in that label, one in the Inbox. A pass
//! must fetch the first two and not the third, and the API reconcile must list the two it
//! fetched, or it would tombstone them.
//!
//! Spends about 20 quota units on each Sent message of the test account. A run that fails
//! part way leaves the planted messages behind, and the next run deletes them first.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_mailbox_window -- --nocapture
//! ```

use std::{collections::BTreeSet, sync::Arc};

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::{MailboxWindows, SyncState, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{
    Credential, CredentialSource, CredentialSubject, EmailChunk, FixedCredential, Provider,
};
use futures_util::StreamExt;
use provider_google::{GmailProvider, GoogleClient};
use provider_imap::{ImapAccount, ImapConfig};
use serde_json::{Value, json};

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The token every planted subject carries, so a rerun can find what an earlier one left.
const MARKER: &str = "mbwgmail";

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn client(token: &str) -> GoogleClient {
    GoogleClient::connect(
        token.to_owned(),
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("http client")
}

async fn get(token: &str, path: &str) -> Value {
    http()
        .get(format!("{API}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json")
}

/// RFC 4648 §5 base64url without padding, as `messages.insert` reads `raw`.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// Files a message dated 2020 under `label` without sending it, and returns its id.
async fn plant(token: &str, label: &str, subject: &str) -> String {
    let source = format!(
        "From: Planted <planted@example.com>\r\nTo: someone@example.com\r\n\
         Subject: {MARKER} {subject}\r\nDate: Mon, 02 Mar 2020 09:00:00 +0000\r\n\
         Message-ID: <{MARKER}-{label}@example.com>\r\n\r\nplanted\r\n"
    );
    let inserted: Value = http()
        .post(format!("{API}/messages?internalDateSource=dateHeader"))
        .bearer_auth(token)
        .json(&json!({ "raw": base64url(source.as_bytes()), "labelIds": [label] }))
        .send()
        .await
        .expect("insert")
        .json()
        .await
        .expect("insert json");
    inserted["id"]
        .as_str()
        .unwrap_or_else(|| panic!("inserted: {inserted}"))
        .to_owned()
}

/// Deletes every message carrying the marker, wherever an earlier run left it.
async fn clear(token: &str) {
    let found = get(
        token,
        &format!("/messages?includeSpamTrash=true&q=subject:{MARKER}"),
    )
    .await;
    for entry in found["messages"].as_array().into_iter().flatten() {
        let id = entry["id"].as_str().expect("id");
        http()
            .delete(format!("{API}/messages/{id}"))
            .bearer_auth(token)
            .send()
            .await
            .expect("delete");
    }
}

/// The account's address, which XOAUTH2 names beside the token.
async fn address(token: &str) -> String {
    get(token, "/profile").await["emailAddress"]
        .as_str()
        .expect("address")
        .to_owned()
}

/// The id of one label of the user's.
async fn user_label(token: &str) -> String {
    get(token, "/labels").await["labels"]
        .as_array()
        .expect("labels")
        .iter()
        .find(|label| label["type"] == "user")
        .and_then(|label| label["id"].as_str())
        .expect("the test account has a label of its own")
        .to_owned()
}

/// Every chunk of one pass from `cursor`.
async fn pass(
    provider: &GmailProvider,
    cursor: Option<&SyncState>,
    window: &MailboxWindows,
) -> Vec<EmailChunk> {
    let account = AccountId::try_from("live").unwrap();
    provider
        .stream_email(&account, cursor, window.clone(), 0, 0)
        .map(|chunk| chunk.expect("a chunk"))
        .collect()
        .await
}

fn fetched(chunks: &[EmailChunk]) -> BTreeSet<String> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.changed)
        .map(|message| message.id.key().as_str().to_owned())
        .collect()
}

fn present(chunks: &[EmailChunk]) -> BTreeSet<String> {
    chunks
        .iter()
        .flat_map(|chunk| &chunk.present)
        .map(|key| key.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn live_a_pass_reaches_a_deepened_label_and_no_other() {
    let Some(token) = token() else {
        eprintln!("skipping live_a_pass_reaches_a_deepened_label_and_no_other: no token");
        return;
    };
    clear(&token).await;
    let label = user_label(&token).await;
    let sent = plant(&token, "SENT", "sent").await;
    let labelled = plant(&token, &label, "labelled").await;
    let inbox = plant(&token, "INBOX", "inbox").await;

    let tomorrow = time::OffsetDateTime::now_utc().date().next_day().unwrap();
    let after_today =
        CalendarDate::new(tomorrow.year(), u8::from(tomorrow.month()), tomorrow.day()).unwrap();
    let back_to = SyncWindow::since(CalendarDate::new(2019, 1, 1).unwrap());
    let window = MailboxWindows::new(SyncWindow::since(after_today))
        .deepen(MailboxId::try_from("SENT").unwrap(), back_to)
        .deepen(MailboxId::try_from(label.as_str()).unwrap(), back_to);

    // Over the API: the backfill, then the reconcile its last checkpoint calls for.
    let api = GmailProvider::new(client(&token));
    let backfill = pass(&api, None, &window).await;
    let over_api = fetched(&backfill);
    let reconcile_at = backfill
        .iter()
        .rev()
        .find_map(|chunk| chunk.advance_to.clone())
        .expect("a checkpoint");
    assert!(
        reconcile_at.as_str().starts_with("reconcile;"),
        "{reconcile_at:?}"
    );
    let listed = present(&pass(&api, Some(&reconcile_at), &window).await);

    // Over IMAP: one snapshot.
    // One source for both halves, as `with_imap` requires.
    let source: Arc<dyn CredentialSource> = Arc::new(FixedCredential::new(
        CredentialSubject::new("live"),
        Credential::bearer_for(address(&token).await, token.clone()),
    ));
    let config = ImapConfig::from_credential_source(
        "imap.gmail.com:993".to_owned(),
        "imap.gmail.com".to_owned(),
        Arc::clone(&source),
    );
    let account = ImapAccount::connect(&config, engine_tls::TlsClientConfig::bundled().connector())
        .await
        .expect("the token must open an IMAP session");
    let sourced = GoogleClient::connect_with_source(
        source,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let imap = GmailProvider::new(sourced)
        .with_imap(account.gmail().expect("X-GM-EXT-1"))
        .expect("one source");
    let over_imap = fetched(&pass(&imap, None, &window).await);

    clear(&token).await;

    for (how, got) in [
        ("API", &over_api),
        ("reconcile", &listed),
        ("IMAP", &over_imap),
    ] {
        assert!(got.contains(&sent), "{how}: Sent's older mail: {got:?}");
        assert!(
            got.contains(&labelled),
            "{how}: the label's older mail: {got:?}"
        );
        assert!(
            !got.contains(&inbox),
            "{how}: the Inbox was not deepened: {got:?}"
        );
    }
}
