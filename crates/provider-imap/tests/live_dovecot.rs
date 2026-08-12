//! Gated live integration: the IMAP folder list against the **Dovecot** harness — the
//! IMAP4rev1 half of the pair whose other half is Stalwart (IMAP4rev2).
//!
//! This file deliberately does **not** re-run `live_imap`'s read/sync assertions against a
//! second server. What earns its round trips is only what the two servers answer
//! *differently*, because that is where a client that quietly assumes one dialect breaks:
//!
//! - an extended `LIST` returns only the extended data its return options name, so the roles arrive
//!   solely because they were asked for (rev2 folds SPECIAL-USE into the base protocol; rev1 makes
//!   it an optional extension, and Stalwart volunteers it either way);
//! - mailbox names travel as **modified UTF-7**, which `UTF8=ACCEPT` on the rev2 harness can never
//!   produce;
//! - the tagged completion is prose (`List completed (0.028 + … secs).`) rather than Stalwart's
//!   two-word `LIST completed`, so a parser that reads the completion line as data invents a
//!   mailbox from it.
//!
//! Skips with no `DOVECOT_IMAP_ADDR`, so the offline `cargo test --workspace` stays green.
//! Per the determinism rule, every assertion is on harness-controlled content (roles,
//! names, counts), never on server-assigned UIDs.

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::Provider;
use provider_imap::{ImapConfig, ImapProvider};
use tokio_rustls::{TlsConnector, client::TlsStream};

/// The harness's own account. Overridable, but these are the committed throwaway values.
fn coordinates() -> Option<(String, String, String)> {
    let addr = std::env::var("DOVECOT_IMAP_ADDR").ok()?;
    let user = std::env::var("DOVECOT_ACCOUNT").unwrap_or_else(|_| "alice@test.local".to_owned());
    let pass = std::env::var("DOVECOT_PASSWORD").unwrap_or_else(|_| "dovecot-alice-pw".to_owned());
    Some((addr, user, pass))
}

/// Accepts the harness's self-signed certificate. Test-only and deliberately insecure;
/// it never touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// Connects to the Dovecot harness, or `None` to skip (offline gate).
async fn connect(test: &str) -> Option<ImapProvider<TlsStream<tokio::net::TcpStream>>> {
    let Some((addr, user, pass)) = coordinates() else {
        eprintln!("skipping {test}: DOVECOT_IMAP_ADDR unset");
        return None;
    };
    let host = addr.rsplit_once(':').map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(addr.as_str(), host, user, pass);
    Some(
        ImapProvider::connect(
            &config,
            no_verify_connector(),
            MailboxId::try_from("INBOX").unwrap(),
        )
        .await
        .expect("connect to the Dovecot harness"),
    )
}

/// The account's folders as the provider reports them.
async fn folders(provider: &ImapProvider<TlsStream<tokio::net::TcpStream>>) -> Vec<Mailbox> {
    let account = AccountId::try_from("dovecot-harness").unwrap();
    match provider.sync_mailboxes(&account, None).await {
        Ok(scope) => match scope.update {
            SyncUpdate::Snapshot { objects, .. } => objects,
            // `LIST` is a full listing every pass, so the folder scope never deltas.
            SyncUpdate::Delta { .. } => panic!("a folder list is a snapshot, got a delta"),
        },
        Err(err) => panic!("sync_mailboxes failed: {err}"),
    }
}

fn find<'a>(all: &'a [Mailbox], name: &str) -> &'a Mailbox {
    all.iter()
        .find(|mailbox| mailbox.name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
            panic!("no mailbox named {name:?} among {names:?}")
        })
}

#[tokio::test]
async fn the_folder_list_carries_roles_and_counts_in_one_pass() {
    let Some(provider) = connect("the_folder_list_carries_roles_and_counts_in_one_pass").await
    else {
        return;
    };
    let all = folders(&provider).await;

    // The roles arrive only because the request asked for them. On this server the same
    // folder list without the `SPECIAL-USE` return option carries every count and no role
    // at all, which is invisible on the rev2 harness — it volunteers them regardless.
    assert_eq!(find(&all, "Sent").role, Some(MailboxRole::Sent));
    assert_eq!(find(&all, "Drafts").role, Some(MailboxRole::Drafts));
    assert_eq!(find(&all, "Trash").role, Some(MailboxRole::Trash));
    assert_eq!(find(&all, "Junk").role, Some(MailboxRole::Junk));
    assert_eq!(find(&all, "Archive").role, Some(MailboxRole::Archive));
    // INBOX is matched by its reserved name, never by an attribute (RFC 9051 §5.1).
    assert_eq!(find(&all, "INBOX").role, Some(MailboxRole::Inbox));

    // …and the counts the same request asked for, in the same pass. The seed puts every
    // message in INBOX unread and one in Sent.
    assert_eq!(find(&all, "INBOX").unread_count, Some(9));
    assert_eq!(find(&all, "Sent").unread_count, Some(1));
    // Zero is a real answer and must not be confused with "the server did not say".
    assert_eq!(find(&all, "Trash").unread_count, Some(0));
}

#[tokio::test]
async fn no_mailbox_is_invented_from_the_completion_line() {
    let Some(provider) = connect("no_mailbox_is_invented_from_the_completion_line").await else {
        return;
    };
    let all = folders(&provider).await;

    // This server completes a `LIST` with `List completed (0.028 + 0.000 + 0.027 secs).`
    // — four items whose first word is the keyword and whose last is a bare `.` sitting
    // where the mailbox name goes. Read as data it becomes a folder the user can see,
    // which then takes a sync scope of its own.
    let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
    assert!(!names.contains(&"."), "invented a mailbox: {names:?}");
    assert_eq!(names.len(), 7, "exactly the seeded mailboxes: {names:?}");
}

#[tokio::test]
async fn a_modified_utf7_mailbox_name_is_decoded_for_display() {
    let Some(provider) = connect("a_modified_utf7_mailbox_name_is_decoded_for_display").await
    else {
        return;
    };
    let all = folders(&provider).await;

    // A rev1 server encodes a non-ASCII mailbox name as modified UTF-7 (RFC 3501 §5.1.3);
    // the rev2 harness negotiates `UTF8=ACCEPT` and sends raw UTF-8, so this path has no
    // live coverage there at all.
    let decoded = find(&all, "Überweisungen");
    // The id stays the **wire** name: it is what `SELECT`/`APPEND` take and what a
    // message key embeds, so decoding it would address a mailbox that does not exist.
    assert_eq!(decoded.id.as_str(), "&ANw-berweisungen");
    assert_eq!(decoded.role, None);
}
