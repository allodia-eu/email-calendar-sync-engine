//! Gated live integration: the IMAP read/sync loop against the Stalwart harness.
//!
//! Connects over implicit TLS (trusting the harness's self-signed test cert via a
//! test-only no-verify verifier — never a host trust store), drives `engine-sync`
//! with the real `ImapProvider` into a real `SqliteStore`, and asserts the seed
//! invariants *in the store*: the INBOX message set, the duplicate `Message-ID`
//! pair as two distinct objects, the flagged keywords, and — the IMAP identity
//! contrast to JMAP — that the COPY in Archive is a **separate** object with its own
//! synthesized key. Skips with no `STALWART_IMAP_ADDR`, so the offline
//! `cargo test --workspace` stays green.
//!
//! Per the determinism rule, every assertion is on harness-controlled content
//! (roles, names, subjects, `Message-ID`s, counts), never on server-assigned UIDs.

use core::time::Duration;
use std::{sync::Mutex, time::Duration as StdDuration};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Mailbox, MailboxRole, Message, SystemKeyword},
    sync::{SyncScope, SyncUpdate},
};
use engine_provider::{Draft, Provider};
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{StreamTuning, SyncCommit, submit_mail, sync_mail, sync_mail_streamed};
use provider_imap::{ImapConfig, ImapProvider};
use serde::de::DeserializeOwned;
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::{TlsConnector, client::TlsStream};

type Store = SqliteStore<ManualClock>;

/// Builds a TLS connector that accepts the harness's self-signed certificate.
/// Test-only and deliberately insecure; it never touches a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// Connects an `ImapProvider` bound to `mailbox`.
async fn connect(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

/// Connects an `ImapProvider` bound to `mailbox` with SMTP submission enabled.
async fn connect_submitter(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    )
    .with_smtp(harness.smtp_addr.as_str());
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP+SMTP")
}

async fn load<T: DeserializeOwned>(store: &Store, scope: &SyncScope, key: &ProviderKey) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

async fn messages_in(store: &Store, scope: &SyncScope) -> Vec<Message> {
    let mut out = Vec::new();
    for key in store.object_keys(scope).await.unwrap() {
        out.push(load::<Message>(store, scope, &key).await);
    }
    out
}

/// Discovers the account's folder carrying `role` (its SPECIAL-USE name — Stalwart
/// calls Sent "Sent Items"), falling back to `default_name` when the server
/// advertises none. This mirrors how the provider resolves its filing target, so
/// the test binds and re-syncs the *same* folder the copy is filed into.
async fn resolve_role_mailbox(harness: &Harness, role: MailboxRole, default_name: &str) -> String {
    let provider = connect(harness, "INBOX").await;
    let account = AccountId::try_from("imap-resolve").unwrap();
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
async fn live_imap_sync_loads_the_inbox_seed() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_sync_loads_the_inbox_seed: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live").unwrap();

    // ---- Sync the INBOX (folder list + its email). ----
    let inbox_provider = connect(&harness, "INBOX").await;

    // Against a *real* server, the post-connect object reports the version the
    // handshake actually agreed. Both sides are rustls, so it is TLS 1.3; IMAP is not
    // HTTP, so that fact is absent rather than unobserved (`docs/agent-guidance/tls.md`).
    let info = inbox_provider.connection_info();
    assert_eq!(info.tls_version, Some(engine_provider::TlsVersion::Tls1_3));
    assert_eq!(info.http_version, None);

    sync_mail(
        &inbox_provider,
        &store,
        &account,
        WorkerId::new("imap-live"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync INBOX");

    // Folders: INBOX/Archive/Projects discovered under the per-account list scope.
    let mailbox_scope = inbox_provider.mailbox_scope(&account);
    let mut folder_roles = Vec::new();
    let mut folder_names = Vec::new();
    for key in store.object_keys(&mailbox_scope).await.unwrap() {
        let mailbox: Mailbox = load(&store, &mailbox_scope, &key).await;
        folder_names.push(mailbox.name.clone());
        if let Some(role) = mailbox.role {
            folder_roles.push(role);
        }
    }
    assert!(
        folder_roles.contains(&MailboxRole::Inbox),
        "INBOX role present"
    );
    assert!(folder_names.iter().any(|n| n == "Archive"));
    assert!(folder_names.iter().any(|n| n == "Projects"));

    // INBOX email: the eight seeded messages (the COPY stays, the MOVE left).
    let inbox_scope = inbox_provider.email_scope(&account);
    let inbox_messages = messages_in(&store, &inbox_scope).await;
    assert_eq!(inbox_messages.len(), 8, "eight messages in the INBOX");

    let subjects: Vec<&str> = inbox_messages
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    for expected in [
        "Harness baseline message",
        "Duplicate Message-ID (copy A)",
        "Duplicate Message-ID (copy B)",
    ] {
        assert!(subjects.contains(&expected), "missing subject {expected:?}");
    }

    // The duplicate Message-ID is two distinct stored objects sharing the hint.
    let dup: Vec<&Message> = inbox_messages
        .iter()
        .filter(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == "shared-dup-msgid@example.com")
        })
        .collect();
    assert_eq!(dup.len(), 2, "duplicate Message-ID → two objects");
    assert_ne!(dup[0].id, dup[1].id);

    // The flagged message carries both the system flag and the custom keyword.
    let flagged = inbox_messages
        .iter()
        .find(|m| m.envelope.subject.as_deref() == Some("Message with flags and a custom keyword"))
        .expect("flagged seed message");
    assert!(flagged.has_system_keyword(SystemKeyword::Flagged));
    assert!(flagged.has_keyword(&Keyword::new("harness").unwrap()));

    // ---- The IMAP identity contrast: the COPY in Archive is a SEPARATE object. ----
    let archive_provider = connect(&harness, "Archive").await;
    sync_mail(
        &archive_provider,
        &store,
        &account,
        WorkerId::new("imap-live"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync Archive");

    let archive_scope = archive_provider.email_scope(&account);
    let archive_messages = messages_in(&store, &archive_scope).await;
    assert_eq!(archive_messages.len(), 1, "the baseline copy in Archive");

    let inbox_baseline = inbox_messages
        .iter()
        .find(|m| m.envelope.subject.as_deref() == Some("Harness baseline message"))
        .unwrap();
    let archive_baseline = &archive_messages[0];
    // Same content/Message-ID hint, but DISTINCT provider identity and a single
    // membership each — unlike JMAP, where the copy is one multi-membership object.
    assert_eq!(
        archive_baseline.envelope.subject.as_deref(),
        Some("Harness baseline message")
    );
    assert_ne!(
        inbox_baseline.id, archive_baseline.id,
        "an IMAP copy in another folder is a distinct object"
    );
    assert!(archive_baseline.id.as_str().contains("@Archive"));
    assert_eq!(archive_baseline.mailboxes.len().get(), 1);
}

#[tokio::test]
async fn live_imap_streams_the_inbox_with_progress() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_streams_the_inbox_with_progress: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = connect(&harness, "INBOX").await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live-stream").unwrap();

    // Fetch batch 2 over the eight-message INBOX forces several committed chunks.
    let recorded: Mutex<Vec<(usize, Option<usize>, SyncScope)>> = Mutex::new(Vec::new());
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-live-stream"),
        Duration::from_mins(5),
        StreamTuning::new(2, 0),
        &|commit: &SyncCommit<'_>| {
            recorded
                .lock()
                .unwrap()
                .push((commit.fetched, commit.total, commit.scope.clone()));
        },
    )
    .await
    .expect("sync_mail_streamed");

    let scope = provider.email_scope(&account);
    let stored = store.object_keys(&scope).await.unwrap().len();
    assert_eq!(stored, 8);
    assert_eq!(report.email.upserted, 8);

    let seq = recorded.lock().unwrap();
    assert!(seq.len() >= 2, "several committed chunks");
    assert!(
        seq.iter().any(|(fetched, ..)| *fetched < stored),
        "an intermediate report"
    );
    assert!(seq.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(seq.iter().all(|(_, _, s)| *s == scope));
    assert_eq!(seq.last().unwrap().1, Some(stored));
    assert_eq!(seq.last().unwrap().0, stored);
}

#[tokio::test]
async fn live_smtp_submits_and_files_the_sent_copy() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_smtp_submits_and_files_the_sent_copy: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    // Bind to the account's real Sent folder (its `\Sent` SPECIAL-USE name —
    // "Sent Items" on Stalwart, not a literal "Sent"), so the copy the provider
    // files there can be re-synced. SMTP submission enabled.
    let sent_mailbox = resolve_role_mailbox(&harness, MailboxRole::Sent, "Sent").await;
    let provider = connect_submitter(&harness, &sent_mailbox).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live-smtp").unwrap();

    let message_id = "step5-imap-smtp-send@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        "Step 5 IMAP/SMTP submission",
        "Sent by the provider-imap live test.",
    );

    // Submit through the outbox: a durable op, then the SMTP send + Sent APPEND.
    let outcome = submit_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-live-smtp"),
        Duration::from_mins(5),
        &draft,
    )
    .await
    .expect("submit_mail");
    assert_eq!(outcome.message_id.as_str(), message_id);

    // The sent copy is filed in Sent and reconciles by the generated Message-ID.
    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-live-smtp"),
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
        "the sent message is filed in Sent, found by its generated Message-ID"
    );
}

#[tokio::test]
async fn live_imap_saves_a_draft() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_saves_a_draft: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    // Bind to the account's real Drafts folder (its `\Drafts` SPECIAL-USE name), so
    // the draft the provider files there can be re-synced. No SMTP needed.
    let drafts_mailbox = resolve_role_mailbox(&harness, MailboxRole::Drafts, "Drafts").await;
    let provider = connect(&harness, &drafts_mailbox).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-live-draft").unwrap();

    let message_id = "step5-imap-draft@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        "Step 5 IMAP draft",
        "Saved as a draft by the live test (no SMTP).",
    );

    // Save it via IMAP APPEND into the resolved Drafts folder (flagged \Draft).
    provider.save_draft(&draft).await.expect("save_draft");

    // Re-sync Drafts; the saved draft is there, found by Message-ID and flagged.
    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("imap-live-draft"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync Drafts");

    let scope = provider.email_scope(&account);
    let saved = messages_in(&store, &scope)
        .await
        .into_iter()
        .find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .expect("the saved draft is in Drafts");
    assert!(saved.is_draft(), "the saved message is flagged \\Draft");
}

// The self-signed harness cert is trusted via `engine_tls`'s test-only
// accept-any config (see `no_verify_connector`), so no local verifier lives here.
