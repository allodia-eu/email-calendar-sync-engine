//! Offline tests for the `Provider` impl, driven over a mock stream.

use super::{ImapConfig, ImapProvider};
use crate::mock::{MockStream, script, written};
use crate::transport::Connection;
use engine_core::ids::{AccountId, MailboxId, MessageIdHeader};
use engine_core::mail::{EmailAddress, MailboxRole};
use engine_core::sync::SyncScope;
use engine_provider::{Draft, Provider};

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
    assert!(provider.capabilities().mail());
    // Mail writes (STORE/MOVE/EXPUNGE) need no extra config, so every IMAP provider
    // advertises them — unlike submission, which is gated on a configured SMTP.
    assert!(provider.capabilities().mail_writes());
    assert!(!provider.capabilities().submission());
    assert!(!provider.capabilities().calendars());
    // This provider's connection never ran CAPABILITY negotiation, so push (IDLE) is
    // not advertised — it is gated on the server, like submission is on SMTP.
    assert!(!provider.capabilities().idle());
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
        provider.capabilities().idle(),
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
async fn sync_email_page_returns_a_snapshot_page() {
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n\
                  * OK [UIDNEXT 4] n\r\na2 OK [READ-WRITE] done\r\n";
    let fetch = "* 1 FETCH (UID 1 FLAGS () ENVELOPE (NIL \"a\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 2 FETCH (UID 2 FLAGS () ENVELOPE (NIL \"b\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 3 FETCH (UID 3 FLAGS () ENVELOPE (NIL \"c\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 a3 OK FETCH done\r\n";
    let provider = connected_provider(script(&[GREETING, LOGIN_OK, select, fetch])).await;

    let page = provider
        .sync_email_page(&account(), None, None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, engine_provider::SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 3);
    assert_eq!(page.present.len(), 3);
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn the_drain_default_merges_pages_into_one_snapshot() {
    // `sync_email` (the trait default) drains `sync_email_page`; with the seed
    // fitting one window it is a single snapshot update.
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n\
                  * OK [UIDNEXT 4] n\r\na2 OK [READ-WRITE] done\r\n";
    let fetch = "* 1 FETCH (UID 1 FLAGS () ENVELOPE (NIL \"a\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 2 FETCH (UID 2 FLAGS () ENVELOPE (NIL \"b\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 * 3 FETCH (UID 3 FLAGS () ENVELOPE (NIL \"c\" NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
                 a3 OK FETCH done\r\n";
    let provider = connected_provider(script(&[GREETING, LOGIN_OK, select, fetch])).await;

    let sync = provider.sync_email(&account(), None).await.unwrap();
    assert!(sync.is_snapshot());
    assert_eq!(sync.next_cursor.as_str(), "v1000;n4");
}

#[tokio::test]
async fn provider_is_object_safe() {
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    let _boxed: Box<dyn Provider> = Box::new(provider);
}

#[tokio::test]
async fn submit_over_smtp_delivers_and_files_the_sent_copy() {
    // The IMAP side files the Sent copy: LIST resolves the real `\Sent` folder
    // (no CREATE needed), then APPEND (with APPENDUID).
    let imap = script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 50 9] APPEND completed\r\n",
    ]);
    let provider = connected_provider(imap).await;

    // The SMTP side delivers cleanly.
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (smtp_stream, smtp_recorded) = MockStream::new(smtp);

    let receipt = provider
        .submit_over(smtp_stream, &submit_draft(), None)
        .await
        .unwrap();

    // The receipt carries the real Sent key from APPENDUID, and the sent Message-ID.
    assert_eq!(receipt.email_key.as_str(), "imap:v50:u9@Sent");
    assert_eq!(receipt.message_id.as_str(), "offline-send@host");
    assert!(written(&smtp_recorded).contains("MAIL FROM:<alice@test.local>"));
}

#[tokio::test]
async fn submit_over_hides_bcc_on_the_wire_but_keeps_it_in_the_sent_copy() {
    // Build the provider over a RECORDED IMAP stream so we can inspect the Sent-copy APPEND.
    // The script: greeting + login (consumed by `login`), then LIST resolves `\Sent` and the
    // APPEND literal is accepted.
    let (imap_stream, imap_recorded) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 50 9] APPEND completed\r\n",
    ]));
    let mut conn = Connection::open(imap_stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    // One reply per command: greeting, EHLO, MAIL, then a RCPT for EACH of To+Cc+Bcc
    // (three), DATA, queued, bye.
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "250 2.1.5 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (smtp_stream, smtp_recorded) = MockStream::new(smtp);

    let draft = submit_draft()
        .with_cc(vec![EmailAddress::new("carol@test.local")])
        .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    provider
        .submit_over(smtp_stream, &draft, None)
        .await
        .unwrap();

    // --- The over-the-wire message (what recipients receive) ---
    let conversation = written(&smtp_recorded);
    // Every recipient — To, Cc, AND Bcc — gets an envelope `RCPT TO`.
    assert!(
        conversation.contains("RCPT TO:<bob@test.local>\r\n"),
        "{conversation}"
    );
    assert!(
        conversation.contains("RCPT TO:<carol@test.local>\r\n"),
        "{conversation}"
    );
    assert!(
        conversation.contains("RCPT TO:<dave@test.local>\r\n"),
        "{conversation}"
    );
    // The transmitted message carries a visible `Cc:` header but NEVER a `Bcc:` one.
    assert!(
        conversation.contains("Cc: carol@test.local\r\n"),
        "{conversation}"
    );
    assert!(!conversation.contains("Bcc:"), "{conversation}");
    // The Cc address appears twice (the envelope `RCPT TO` AND the `Cc:` header), but the Bcc
    // address appears exactly ONCE — only in the envelope, never in the transmitted message —
    // so no recipient can see it.
    assert_eq!(
        conversation.matches("carol@test.local").count(),
        2,
        "{conversation}"
    );
    assert_eq!(
        conversation.matches("dave@test.local").count(),
        1,
        "{conversation}"
    );

    // --- The filed Sent copy (what the SENDER keeps) ---
    // The APPENDed Sent copy DOES carry the `Bcc:` header, so the sender's Sent folder records
    // whom they Bcc'd — the other half of the Outlook/Thunderbird behavior.
    let appended = written(&imap_recorded);
    assert!(appended.contains("Bcc: dave@test.local\r\n"), "{appended}");
    assert!(appended.contains("Cc: carol@test.local\r\n"), "{appended}");
}

#[tokio::test]
async fn submit_over_deduplicates_a_recipient_listed_in_both_to_and_cc() {
    let provider = connected_provider(script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK send literal\r\n",
        "a3 OK [APPENDUID 50 9] APPEND completed\r\n",
    ]))
    .await;
    // Exactly ONE RCPT reply: bob is in both To and Cc but the envelope de-duplicates him.
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (smtp_stream, smtp_recorded) = MockStream::new(smtp);

    // submit_draft()'s To is bob@test.local; adding him to Cc must not yield a second RCPT.
    let draft = submit_draft().with_cc(vec![EmailAddress::new("bob@test.local")]);
    provider
        .submit_over(smtp_stream, &draft, None)
        .await
        .unwrap();

    let conversation = written(&smtp_recorded);
    assert_eq!(
        conversation.matches("RCPT TO:").count(),
        1,
        "{conversation}"
    );
}

#[tokio::test]
async fn submit_over_smtp_maps_a_lost_ack_to_needs_confirmation() {
    // SMTP fails (lost post-DATA ack) before the Sent APPEND, so the IMAP side is
    // only greeted and logged in.
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        // EOF: no post-DATA reply.
    ]);
    let (smtp_stream, _) = MockStream::new(smtp);

    let err = provider
        .submit_over(smtp_stream, &submit_draft(), None)
        .await
        .unwrap_err();
    assert!(
        err.requires_confirmation(),
        "ambiguity must need confirmation"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn submit_over_smtp_rejects_permanently_when_no_recipient_accepts() {
    let provider = connected_provider(script(&[GREETING, LOGIN_OK])).await;
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "550 5.1.2 no such mailbox\r\n", // the only recipient is rejected
    ]);
    let (smtp_stream, _) = MockStream::new(smtp);

    let err = provider
        .submit_over(smtp_stream, &submit_draft(), None)
        .await
        .unwrap_err();
    // A permanent rejection is neither retryable nor a confirmation case.
    assert!(!err.is_retryable());
    assert!(!err.requires_confirmation());
}

#[tokio::test]
async fn submit_falls_back_to_a_message_id_key_without_appenduid() {
    // APPEND succeeds but the server returns no APPENDUID → a Message-ID-derived key.
    let imap = script(&[
        GREETING,
        LOGIN_OK,
        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\na2 OK LIST done\r\n",
        "+ OK\r\n",
        "a3 OK APPEND completed\r\n", // no [APPENDUID]
    ]);
    let provider = connected_provider(imap).await;
    let smtp = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (smtp_stream, _) = MockStream::new(smtp);

    let receipt = provider
        .submit_over(smtp_stream, &submit_draft(), None)
        .await
        .unwrap();
    assert_eq!(receipt.email_key.as_str(), "sent:offline-send@host");
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
    );
    assert!(provider.capabilities().submission());

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
    assert!(!provider.capabilities().submission());
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
