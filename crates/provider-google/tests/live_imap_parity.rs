//! Gated live proof that a Gmail sync read over IMAP writes what the API sync writes.
//!
//! The IMAP pass (`imap_pass`) exists to replace the API's per-message reads, and an account
//! moves between the two without a resync of its rows only if both modes produce the same
//! message under the same key. So this syncs one real account both ways, the whole mailbox
//! with no window, and compares every message: its key, its thread, its memberships and its
//! keywords. It also prints how long each took, which is the reason for the IMAP pass.
//!
//! Read-only on both sides. Run it alone: the API half spends 20 units of the account's 6,000
//! per minute on every message (`src/quota.rs`).
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_imap_parity -- --nocapture
//! ```

use std::{collections::BTreeMap, time::Instant};

use engine_core::{
    ids::AccountId,
    mail::{Message, ThreadRef},
    sync::SyncWindow,
};
use engine_provider::Provider;
use futures_util::StreamExt;
use provider_google::{GmailProvider, GoogleClient};
use provider_imap::{Credentials, ImapAccount, ImapConfig};

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

/// The account's address, which XOAUTH2 names beside the token.
async fn address(token: &str) -> String {
    let profile: serde_json::Value = engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("http client")
        .get("https://gmail.googleapis.com/gmail/v1/users/me/profile")
        .bearer_auth(token)
        .send()
        .await
        .expect("profile")
        .json()
        .await
        .expect("profile json");
    profile["emailAddress"]
        .as_str()
        .expect("address")
        .to_owned()
}

/// What a sync wrote of each message, by key.
type Written = BTreeMap<String, (Option<ThreadRef>, Vec<String>, Vec<String>)>;

async fn sync(provider: &GmailProvider) -> Written {
    sync_messages(provider).await.0
}

async fn sync_messages(provider: &GmailProvider) -> (Written, Vec<Message>) {
    let account = AccountId::try_from("live").unwrap();
    let mut written = Written::new();
    let mut messages = Vec::new();
    let mut chunks = provider.stream_email(&account, None, SyncWindow::full(), 0, 0);
    while let Some(chunk) = chunks.next().await {
        for message in chunk.expect("a pass that completes").changed {
            written.insert(message.id.key().as_str().to_owned(), facts(&message));
            messages.push(message);
        }
    }
    (written, messages)
}

fn facts(message: &Message) -> (Option<ThreadRef>, Vec<String>, Vec<String>) {
    (
        message.thread.clone(),
        message
            .mailboxes
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
        message.keywords.iter().map(|k| format!("{k:?}")).collect(),
    )
}

#[tokio::test]
async fn an_imap_sync_writes_what_the_api_sync_writes() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let config = ImapConfig::new(
        "imap.gmail.com:993".to_owned(),
        "imap.gmail.com".to_owned(),
        Credentials::oauth2(address(&token).await, token.clone()),
    );
    let account = ImapAccount::connect(&config, engine_tls::TlsClientConfig::bundled().connector())
        .await
        .expect("the token must open an IMAP session");
    let gmail = account
        .gmail()
        .expect("imap.gmail.com advertises X-GM-EXT-1");

    let imap = GmailProvider::new(client(&token)).with_imap(gmail);
    let started = Instant::now();
    let (over_imap, messages) = sync_messages(&imap).await;
    let imap_took = started.elapsed();

    // Bodies arrive one at a time off the wire, each under its own message, all of them.
    let batch = &messages[..messages.len().min(60)];
    let started = Instant::now();
    let account = AccountId::try_from("live").unwrap();
    let mut seen = vec![0usize; batch.len()];
    let mut sources = imap.fetch_message_sources(&account, batch);
    let mut bytes = 0;
    while let Some((index, source)) = sources.next().await {
        let source = source.expect("every listed message has a body");
        assert_ne!(source.as_bytes(), &[] as &[u8]);
        bytes += source.as_bytes().len();
        seen[index] += 1;
    }
    assert!(seen.iter().all(|&n| n == 1), "each body once: {seen:?}");
    println!(
        "{} bodies, {bytes} octets, in {:.2}s",
        batch.len(),
        started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    let over_api = sync(&GmailProvider::new(client(&token))).await;
    let api_took = started.elapsed();
    println!(
        "{} messages: IMAP {:.2}s, API {:.2}s",
        over_imap.len(),
        imap_took.as_secs_f64(),
        api_took.as_secs_f64()
    );

    assert!(
        !over_api.is_empty(),
        "the live account should hold some mail"
    );
    let only_api: Vec<&String> = over_api
        .keys()
        .filter(|k| !over_imap.contains_key(*k))
        .collect();
    let only_imap: Vec<&String> = over_imap
        .keys()
        .filter(|k| !over_api.contains_key(*k))
        .collect();
    assert!(
        only_api.is_empty() && only_imap.is_empty(),
        "API only: {only_api:?}; IMAP only: {only_imap:?}"
    );
    let differing: Vec<_> = over_api
        .iter()
        .filter(|(key, api)| over_imap[*key] != **api)
        .map(|(key, api)| (key, api, &over_imap[key]))
        .collect();
    assert!(
        differing.is_empty(),
        "{} differ, e.g. {:#?}",
        differing.len(),
        differing.first()
    );
}
