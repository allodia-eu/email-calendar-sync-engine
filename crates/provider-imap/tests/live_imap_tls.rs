//! Gated live integration: the IMAP/SMTP **TLS transports beyond `live_imap.rs`'s
//! baseline** (which covers implicit-TLS IMAP on 993 + plaintext SMTP on 25).
//!
//! - **STARTTLS** — an IMAP STARTTLS dial (143) and an SMTP submission STARTTLS send (587).
//!   Stalwart supports these ports but recommends implicit TLS, so a fresh bootstrap does not
//!   create them; the harness entrypoint provisions them. These exercise the cleartext-connect →
//!   `STARTTLS` upgrade → authenticated session path the offline mocks cannot validate (they serve
//!   canned bytes regardless of request).
//! - **Implicit-TLS submission** — an SMTP submission over implicit TLS + `AUTH PLAIN` (465, a
//!   Stalwart default listener), the `with_smtp_tls` path that otherwise has only offline coverage.
//!
//! All trust the harness's self-signed cert via a test-only no-verify verifier — never
//! a host trust store. Skips with no `STALWART_HTTP_ADDR`, so the offline
//! `cargo test --workspace` stays green with no Docker.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, MailboxRole, StoredContent},
    sync::{SyncScope, SyncUpdate},
};
use engine_provider::{Draft, Provider};
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{submit_mail, sync_mail};
use provider_imap::{ImapConfig, ImapProvider};
use serde::de::DeserializeOwned;
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::{TlsConnector, client::TlsStream};

type Store = SqliteStore<ManualClock>;

/// A TLS connector that accepts the harness's self-signed certificate. Test-only and
/// deliberately insecure; it never touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// The host portion of a `host:port` address (the TLS SNI/cert name).
fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or("localhost", |(host, _)| host)
}

/// Connects an `ImapProvider` bound to `mailbox` over **STARTTLS** (port 143).
async fn connect_starttls(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let config = ImapConfig::new(
        harness.imap_starttls_addr.as_str(),
        host_of(&harness.imap_starttls_addr),
        harness.account.as_str(),
        harness.password.as_str(),
    )
    .with_starttls();
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP STARTTLS")
}

/// Connects a STARTTLS `ImapProvider` bound to `mailbox` with **SMTP submission over
/// STARTTLS** (port 587, `AUTH PLAIN`) enabled.
async fn connect_starttls_submitter(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let config = ImapConfig::new(
        harness.imap_starttls_addr.as_str(),
        host_of(&harness.imap_starttls_addr),
        harness.account.as_str(),
        harness.password.as_str(),
    )
    .with_starttls()
    .with_smtp_starttls(
        harness.smtp_starttls_addr.as_str(),
        host_of(&harness.smtp_starttls_addr),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP+SMTP STARTTLS")
}

/// Connects an **implicit-TLS** IMAP provider (993) bound to `mailbox` with **SMTP
/// submission over implicit TLS** (port 465, `AUTH PLAIN`) enabled — the `with_smtp_tls`
/// path (not STARTTLS; the IMAP side stays on 993).
async fn connect_tls_submitter(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host_of(&harness.imap_addr),
        harness.account.as_str(),
        harness.password.as_str(),
    )
    .with_smtp_tls(
        harness.smtp_tls_addr.as_str(),
        host_of(&harness.smtp_tls_addr),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP + implicit-TLS SMTP")
}

async fn load<T: DeserializeOwned>(store: &Store, scope: &SyncScope, key: &ProviderKey) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

/// The account's stored payloads, decoded.
///
/// [`StoredContent`], not `Message`: a payload is a message's immutable half, and the mutable
/// half — keywords, filing, revision tokens — lives in the `message` row and the `membership`
/// junction. Assert state against the rows, never against these.
async fn messages_in(store: &Store, scope: &SyncScope) -> Vec<StoredContent> {
    let mut out = Vec::new();
    for key in store.object_keys(scope).await.unwrap() {
        out.push(load::<StoredContent>(store, scope, &key).await);
    }
    out
}

/// Discovers the account's folder carrying `role` (its SPECIAL-USE name), falling back
/// to `default_name`. Mirrors how the provider resolves its filing target.
async fn resolve_role_mailbox(harness: &Harness, role: MailboxRole, default_name: &str) -> String {
    let provider = connect_starttls(harness, "INBOX").await;
    let account = AccountId::try_from("imap-starttls-resolve").unwrap();
    let sync = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("list folders");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        return default_name.to_owned();
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role.as_ref() == Some(&role))
        .map_or_else(|| default_name.to_owned(), |mailbox| mailbox.name)
}

#[tokio::test]
async fn live_imap_starttls_syncs_the_inbox_seed() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_starttls_syncs_the_inbox_seed: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = connect_starttls(&harness, "INBOX").await;

    // The upgrade negotiated a TLS version — proof the cleartext socket became TLS
    // before login (an implicit-TLS dial reports the same, but here it can only be
    // non-`None` if STARTTLS succeeded, since the connection started in the clear).
    assert!(
        provider.connection_info().tls_version.is_some(),
        "STARTTLS must negotiate a TLS version"
    );

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-starttls").unwrap();

    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-starttls"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync over STARTTLS");

    // The upgraded session runs real IMAP: the eight-message INBOX seed lands.
    let scope = provider.email_scope(&account);
    assert_eq!(store.object_keys(&scope).await.unwrap().len(), 8);
}

#[tokio::test]
async fn live_smtp_starttls_submits_and_files_the_sent_copy() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping live_smtp_starttls_submits_and_files_the_sent_copy: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    // Bind to the account's real Sent folder so the filed copy can be re-synced. IMAP
    // over STARTTLS (143) and submission over STARTTLS + AUTH PLAIN (587).
    let sent_mailbox = resolve_role_mailbox(&harness, MailboxRole::Sent, "Sent").await;
    let provider = connect_starttls_submitter(&harness, &sent_mailbox).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-starttls-smtp").unwrap();

    let message_id = "starttls-imap-smtp-send@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        "STARTTLS submission",
        "Sent over SMTP STARTTLS by the provider-imap live test.",
    );

    let outcome = submit_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-starttls-smtp"),
        Duration::from_mins(5),
        &draft,
    )
    .await
    .expect("submit_mail over STARTTLS");
    assert_eq!(outcome.message_id.as_str(), message_id);

    // The sent copy is filed in Sent and reconciles by the generated Message-ID.
    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-starttls-smtp"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync Sent");

    let sent_scope = provider.email_scope(&account);
    let sent = messages_in(&store, &sent_scope).await;
    assert!(
        sent.iter().any(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        }),
        "the STARTTLS-submitted message is filed in Sent, found by its Message-ID"
    );
}

#[tokio::test]
async fn live_smtp_implicit_tls_submits_and_files_the_sent_copy() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping live_smtp_implicit_tls_submits_and_files_the_sent_copy: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    // SMTP submission over **implicit TLS** (port 465, a Stalwart default listener) with
    // `AUTH PLAIN` — the `with_smtp_tls` path, otherwise only offline-tested. IMAP stays
    // on implicit TLS (993).
    let sent_mailbox = resolve_role_mailbox(&harness, MailboxRole::Sent, "Sent").await;
    let provider = connect_tls_submitter(&harness, &sent_mailbox).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-implicit-tls-smtp").unwrap();

    let message_id = "implicit-tls-imap-smtp-send@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        "Implicit-TLS submission",
        "Sent over SMTP implicit TLS + AUTH PLAIN by the provider-imap live test.",
    );

    let outcome = submit_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-implicit-tls-smtp"),
        Duration::from_mins(5),
        &draft,
    )
    .await
    .expect("submit_mail over implicit TLS");
    assert_eq!(outcome.message_id.as_str(), message_id);

    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-implicit-tls-smtp"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync Sent");

    let sent = messages_in(&store, &provider.email_scope(&account)).await;
    assert!(
        sent.iter().any(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        }),
        "the implicit-TLS-submitted message is filed in Sent, found by its Message-ID"
    );
}
