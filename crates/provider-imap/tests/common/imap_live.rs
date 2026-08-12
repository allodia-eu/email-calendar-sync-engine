//! Shared plumbing for the gated live IMAP suites, which run the **same** assertions
//! against every server configured for them.
//!
//! Each suite reads one `*_IMAP_ADDR` variable per server and skips when it is unset, so
//! the offline `cargo test --workspace` stays green. CI sets one per harness job; locally,
//! set whichever harness is up.

// `#[path]`-included into each gated suite, so every one of them compiles the whole module
// and uses only the part it needs — the contract suite reads `SERVERS`, the dialect suites
// name one server each. The alternative is a shared crate for four constants.
//
// `unreachable_pub` likewise: the items are `pub` because each including test crate reaches
// them through a private `mod`, which is exactly the shape the lint flags and exactly what a
// `#[path]` test helper is.
#![allow(dead_code, unreachable_pub)]

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::Mailbox,
    sync::SyncUpdate,
};
use engine_provider::Provider;
use provider_imap::{ImapConfig, ImapProvider};
use tokio_rustls::{TlsConnector, client::TlsStream};

/// A live IMAP server this suite can run against.
pub struct Server {
    /// The name in skip messages and assertion failures.
    pub label: &'static str,
    /// The environment variable carrying `host:port`; absence is the skip signal.
    pub addr_var: &'static str,
    /// The account, and its throwaway password.
    pub account: &'static str,
    pub password: &'static str,
}

/// The Stalwart harness (`docker/stalwart/`) — IMAP4rev2.
pub const STALWART: Server = Server {
    label: "stalwart",
    addr_var: "STALWART_IMAP_ADDR",
    account: "alice@test.local",
    password: "harness-alice-pw",
};

/// The Dovecot harness (`docker/dovecot/`) — IMAP4rev1.
pub const DOVECOT: Server = Server {
    label: "dovecot",
    addr_var: "DOVECOT_IMAP_ADDR",
    account: "alice@test.local",
    password: "dovecot-alice-pw",
};

/// Every server a suite should run against, skipping those whose variable is unset.
pub const SERVERS: [Server; 2] = [STALWART, DOVECOT];

/// Accepts a harness's self-signed certificate. Test-only and deliberately insecure; it
/// never touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

pub type LiveProvider = ImapProvider<TlsStream<tokio::net::TcpStream>>;

/// Connects to `server` bound to `INBOX`, or `None` when its address variable is unset
/// (the offline gate).
pub async fn connect(server: &Server, test: &str) -> Option<LiveProvider> {
    connect_to(server, "INBOX", test).await
}

/// Connects to `server` bound to `mailbox`, named by its **decoded** identity. Binding is
/// what makes the transport put the wire form back on the `SELECT` that follows.
pub async fn connect_to(server: &Server, mailbox: &str, test: &str) -> Option<LiveProvider> {
    let Ok(addr) = std::env::var(server.addr_var) else {
        eprintln!(
            "skipping {test} on {}: {} unset",
            server.label, server.addr_var
        );
        return None;
    };
    let host = addr.rsplit_once(':').map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(addr.as_str(), host, server.account, server.password);
    Some(
        ImapProvider::connect(
            &config,
            no_verify_connector(),
            MailboxId::try_from(mailbox).unwrap(),
        )
        .await
        .unwrap_or_else(|err| panic!("connect to the {} harness: {err}", server.label)),
    )
}

/// One account's folders as the provider reports them.
pub async fn folders(provider: &LiveProvider) -> Vec<Mailbox> {
    let account = AccountId::try_from("live-harness").unwrap();
    match provider.sync_mailboxes(&account, None).await {
        Ok(scope) => match scope.update {
            SyncUpdate::Snapshot { objects, .. } => objects,
            // `LIST` is a full listing every pass, so the folder scope never deltas.
            SyncUpdate::Delta { .. } => panic!("a folder list is a snapshot, got a delta"),
        },
        Err(err) => panic!("sync_mailboxes failed: {err}"),
    }
}

/// The named mailbox, or a failure naming everything that *was* listed.
pub fn find<'a>(all: &'a [Mailbox], name: &str) -> &'a Mailbox {
    all.iter()
        .find(|mailbox| mailbox.name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
            panic!("no mailbox named {name:?} among {names:?}")
        })
}
