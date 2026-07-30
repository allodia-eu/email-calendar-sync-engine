//! Offline tests for the `Provider` impl, driven over a mock stream.

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole},
    sync::{SyncScope, SyncWindow},
};
use engine_provider::{Draft, EmailChunk, Provider, TlsVersion};
use futures_util::StreamExt;

use super::{ImapConfig, ImapProvider};
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

fn submit_draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("offline-send@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Hi",
        "body",
    )
}

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// Builds a provider bound to INBOX over a mock that has greeted and accepted login.
async fn connected_provider(server: Vec<u8>) -> ImapProvider<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap())
}

#[tokio::test]
async fn scopes_are_imap_shaped() {
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    // The folder list is per-account; email is per-mailbox.
    assert_eq!(
        provider.mailbox_scope(&account()),
        SyncScope::ImapMailboxList { account: account() }
    );
    assert_eq!(
        provider.email_scope(&account()),
        SyncScope::ImapMailbox {
            account: account(),
            mailbox: MailboxId::try_from("INBOX").unwrap(),
        }
    );
    assert!(provider.connection_info().capabilities.mail());
    // Mail writes (STORE/MOVE/EXPUNGE) need no extra config, so every IMAP provider
    // advertises them — unlike submission, which is gated on a configured SMTP.
    assert!(provider.connection_info().capabilities.mail_writes());
    assert!(!provider.connection_info().capabilities.submission());
    assert!(!provider.connection_info().capabilities.calendars());
    // This provider's connection never ran CAPABILITY negotiation, so push (IDLE) is
    // not advertised — it is gated on the server, like submission is on SMTP.
    assert!(!provider.connection_info().capabilities.idle());
    // A mock stream ran no handshake, so there is no TLS version to report; a real
    // dial captures one (`tls_info`). IMAP is not HTTP, so that version is never set.
    assert_eq!(provider.connection_info().tls_version, None);
    assert_eq!(provider.connection_info().http_version, None);
}

#[tokio::test]
async fn idle_capability_reflects_a_post_auth_advertisement() {
    // A server that advertises IDLE post-auth (Stalwart, Dovecot, …): negotiation
    // records it, so the built provider advertises push and a host can offer an
    // "as it comes in" strategy. Connection::open consumes the greeting (`a0`),
    // login is `a1`, and CAPABILITY is the next tagged command (`a2`).
    let (stream, _) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        "* CAPABILITY IMAP4rev2 IDLE CONDSTORE QRESYNC\r\na2 OK done\r\n",
        "* ENABLED QRESYNC\r\na3 OK enabled\r\n",
    ]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.negotiate_qresync().await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());
    assert!(
        provider.connection_info().capabilities.idle(),
        "an advertised IDLE becomes the provider's push capability"
    );
}

#[tokio::test]
async fn edit_mail_marks_a_message_read_through_the_provider() {
    // The trait method is a thin lock-and-call into `mutate`: SELECT (UIDVALIDITY
    // guard) then a silent STORE. The receipt carries the target key.
    let select = "* 1 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na2 OK [READ-WRITE] done\r\n";
    let (stream, recorded) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        select,
        "a3 OK STORE done\r\n",
    ]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let target = engine_core::ids::ProviderKey::new("imap:v7:u42@INBOX").unwrap();
    let receipt = provider
        .edit_mail(
            &account(),
            &engine_provider::MailEdit::mark_seen(target.clone(), true),
        )
        .await
        .unwrap();
    assert_eq!(receipt.message_key, target);

    let sent = written(&recorded);
    assert!(sent.contains("a2 SELECT \"INBOX\""), "{sent}");
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT (\\Seen)"),
        "{sent}"
    );
}

#[tokio::test]
async fn sync_mailboxes_lists_folders_as_a_snapshot() {
    let list = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
                * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                a2 OK LIST done\r\n";
    let provider = connected_provider(script(&[GREETING, LOGIN_OK, list])).await;

    let sync = provider.sync_mailboxes(&account(), None).await.unwrap();
    assert!(sync.is_snapshot());
    let names: Vec<_> = match &sync.update {
        engine_core::sync::SyncUpdate::Snapshot { objects, .. } => {
            objects.iter().map(|m| m.name.clone()).collect()
        }
        engine_core::sync::SyncUpdate::Delta { .. } => panic!("expected a snapshot"),
    };
    assert!(names.contains(&"INBOX".to_owned()));
    assert!(names.contains(&"Sent".to_owned()));

    let inbox_role = match &sync.update {
        engine_core::sync::SyncUpdate::Snapshot { objects, .. } => objects
            .iter()
            .find(|m| m.name == "INBOX")
            .and_then(|m| m.role.clone()),
        engine_core::sync::SyncUpdate::Delta { .. } => unreachable!(),
    };
    assert_eq!(inbox_role, Some(MailboxRole::Inbox));
}

#[tokio::test]
async fn a_first_sync_streams_a_resumable_backfill() {
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n\
                  * OK [UIDNEXT 4] n\r\na2 OK [READ-WRITE] done\r\n";
    let fetch = "* 1 FETCH (UID 1 FLAGS () ENVELOPE (NIL \"a\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 2 FETCH (UID 2 FLAGS () ENVELOPE (NIL \"b\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 3 FETCH (UID 3 FLAGS () ENVELOPE (NIL \"c\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 a3 OK FETCH done\r\n";
    let provider = connected_provider(script(&[GREETING, LOGIN_OK, select, fetch])).await;

    let account = account();
    let mut stream = Box::pin(provider.stream_email(&account, None, SyncWindow::full(), 50, 0));
    let mut chunks: Vec<EmailChunk> = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    // A fresh cold backfill streams the content, then its completing chunk reconciles
    // against the full present set (so a reset over a non-empty store tombstones stale
    // rows). Here the three messages fit one fetch group — the last (only) chunk.
    let last = chunks.last().unwrap();
    assert!(
        last.is_reconcile_final(),
        "a fresh backfill reconciles on completion"
    );
    assert_eq!(
        last.present.len(),
        3,
        "the full present set drives tombstoning"
    );
    let upserted: usize = chunks.iter().map(|c| c.changed.len()).sum();
    assert_eq!(upserted, 3);
    assert_eq!(last.advance_to.as_ref().unwrap().as_str(), "v1000;n4");
}

#[tokio::test]
async fn the_drain_default_merges_a_first_sync_into_a_reconciling_snapshot() {
    // `sync_email` (the trait default) drains `stream_email`; a fresh backfill's
    // completing reconcile makes the drained update a snapshot (present-set driven), so
    // a reset over an existing store tombstones — matching JMAP/Graph first-sync.
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n\
                  * OK [UIDNEXT 4] n\r\na2 OK [READ-WRITE] done\r\n";
    let fetch = "* 1 FETCH (UID 1 FLAGS () ENVELOPE (NIL \"a\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 2 FETCH (UID 2 FLAGS () ENVELOPE (NIL \"b\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 3 FETCH (UID 3 FLAGS () ENVELOPE (NIL \"c\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 a3 OK FETCH done\r\n";
    let provider = connected_provider(script(&[GREETING, LOGIN_OK, select, fetch])).await;

    let sync = provider.sync_email(&account(), None).await.unwrap();
    assert!(
        sync.is_snapshot(),
        "a fresh backfill reconciles on completion"
    );
    assert_eq!(sync.next_cursor.as_str(), "v1000;n4");
}

#[tokio::test]
async fn provider_is_object_safe() {
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    let _boxed: Box<dyn Provider> = Box::new(provider);
}

#[tokio::test]
async fn save_draft_creates_drafts_when_no_special_use_folder_exists() {
    // LIST advertises no `\Drafts` folder, so the client falls back to the
    // conventional name: CREATE "Drafts", then APPEND flagged `\Draft`.
    let imap = script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\na2 OK LIST done\r\n",
        "a3 OK CREATE completed\r\n",
        "+ OK send literal\r\n",
        "a4 OK [APPENDUID 70 4] APPEND completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(imap);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let key = provider.save_draft(&submit_draft()).await.unwrap();
    assert_eq!(key.as_str(), "imap:v70:u4@Drafts");

    let sent = written(&recorded);
    assert!(sent.contains("CREATE \"Drafts\""), "{sent}");
    assert!(
        sent.contains("APPEND \"Drafts\" (\\Draft \\Seen)"),
        "{sent}"
    );
}

#[tokio::test]
async fn save_draft_files_into_the_special_use_drafts_folder() {
    // The server names its drafts folder differently and tags it `\Drafts`; the
    // client must file into that real folder (no CREATE), not a stray "Drafts".
    let imap = script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
         * LIST (\\HasNoChildren \\Drafts) \"/\" \"[Mail]/Concepten\"\r\n\
         a2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 70 4] APPEND completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(imap);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let key = provider.save_draft(&submit_draft()).await.unwrap();
    assert_eq!(key.as_str(), "imap:v70:u4@[Mail]/Concepten");

    let sent = written(&recorded);
    // No stray CREATE; the resolved folder is appended to directly.
    assert!(!sent.contains("CREATE"), "{sent}");
    assert!(
        sent.contains("APPEND \"[Mail]/Concepten\" (\\Draft \\Seen)"),
        "{sent}"
    );
}

/// A blocking loopback server that speaks just enough SMTP to accept one message,
/// so `submit_email`'s real `TcpStream::connect` + plaintext dispatch run offline
/// (mirroring `provider-jmap`'s mock HTTP server).
fn loopback_smtp() -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        socket.write_all(b"220 mock ESMTP\r\n").unwrap();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() != 0 {
            let command = line.trim_end().to_uppercase();
            if command == "DATA" {
                socket.write_all(b"354 go ahead\r\n").unwrap();
                let mut body = String::new();
                while reader.read_line(&mut body).unwrap() != 0 {
                    if body == ".\r\n" {
                        break;
                    }
                    body.clear();
                }
                socket.write_all(b"250 2.0.0 queued\r\n").unwrap();
            } else if command == "QUIT" {
                socket.write_all(b"221 bye\r\n").unwrap();
                break;
            } else {
                socket.write_all(b"250 OK\r\n").unwrap();
            }
            line.clear();
        }
    });
    addr
}

#[tokio::test]
async fn submit_email_dispatches_the_plaintext_transport_end_to_end() {
    // IMAP side files the Sent copy (LIST resolves `\Sent`); SMTP side is the
    // loopback server.
    let imap = script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 12 3] APPEND completed\r\n",
    ]);
    let (stream, _) = MockStream::new(imap);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::build(
        conn,
        MailboxId::try_from("INBOX").unwrap(),
        Some(super::SmtpSender::Plaintext {
            addr: loopback_smtp(),
        }),
        None,
        None,
        crate::namespace::Namespaces::default(),
    );
    assert!(provider.connection_info().capabilities.submission());

    let receipt = provider
        .submit_email(&account(), &submit_draft())
        .await
        .unwrap();
    assert_eq!(receipt.email_key.as_str(), "imap:v12:u3@Sent");
    assert_eq!(receipt.message_id.as_str(), "offline-send@host");
}

#[tokio::test]
async fn submit_email_without_a_transport_is_rejected() {
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    let err = provider
        .submit_email(&account(), &submit_draft())
        .await
        .unwrap_err();
    assert!(!provider.connection_info().capabilities.submission());
    assert!(!err.is_retryable());
}

#[test]
fn config_debug_redacts_the_password() {
    let config = ImapConfig::new(
        "127.0.0.1:11993",
        "localhost",
        "alice@test.local",
        "super-secret",
    );
    let shown = format!("{config:?}");
    assert!(shown.contains("alice@test.local"));
    assert!(
        !shown.contains("super-secret"),
        "password must not leak: {shown}"
    );
}

/// Records connect steps as the log lines a host would emit.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl engine_provider::ConnectObserver for Recorder {
    fn step(&self, step: &engine_provider::ConnectStep<'_>) {
        use engine_provider::ConnectStep;
        let line = match step {
            ConnectStep::TlsEstablished(version) => format!("tls {version:?}"),
            ConnectStep::Authenticated => "authenticated".to_owned(),
            other => format!("unexpected {other:?}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

/// Drives the shared dial over a mock stream, returning the steps it reported.
async fn observed_open_session(server_script: Vec<u8>, tls: Option<TlsVersion>) -> Vec<String> {
    let recorder = std::sync::Arc::new(Recorder::default());
    let config = ImapConfig::new("h:993", "h", "alice@test.local", "pw")
        .with_connect_observer(recorder.clone());
    let (stream, _recorded) = MockStream::new(server_script);
    super::open_session(stream, tls, &config)
        .await
        .expect("session");
    let steps = recorder.0.lock().unwrap();
    steps.clone()
}

#[tokio::test]
async fn connect_reports_the_tls_handshake_then_the_login() {
    // The exact sequence, in order: the handshake precedes the greeting, and `LOGIN`
    // precedes the post-auth CAPABILITY (which is extension negotiation, not a step).
    let steps = observed_open_session(
        script(&[
            GREETING,
            LOGIN_OK,
            "* CAPABILITY IMAP4rev2 IDLE\r\na2 OK done\r\n",
        ]),
        Some(TlsVersion::Tls1_3),
    )
    .await;
    assert_eq!(steps, ["tls Tls1_3", "authenticated"]);
}

#[tokio::test]
async fn a_stream_that_is_not_tls_reports_only_the_login() {
    // `tls_version` is `None` when the stream is not TLS — the fact is not applicable,
    // not merely unobserved, so no step is invented for it.
    let steps = observed_open_session(script(&[GREETING, LOGIN_OK, "a2 OK done\r\n"]), None).await;
    assert_eq!(steps, ["authenticated"]);
}

#[tokio::test]
async fn a_failed_login_reports_the_handshake_but_never_authentication() {
    // `Authenticated` means the server accepted the credentials. A `NO` must not emit
    // it — a host driving a state machine off these steps would otherwise believe a
    // rejected connection came up.
    let recorder = std::sync::Arc::new(Recorder::default());
    let config = ImapConfig::new("h:993", "h", "alice@test.local", "wrong")
        .with_connect_observer(recorder.clone());
    let (stream, _recorded) = MockStream::new(script(&[GREETING, "a1 NO bad credentials\r\n"]));
    let err = super::open_session(stream, Some(TlsVersion::Tls1_2), &config)
        .await
        .expect_err("login must fail");
    assert!(matches!(err, crate::error::ImapError::Auth(_)));
    assert_eq!(*recorder.0.lock().unwrap(), ["tls Tls1_2"]);
}
