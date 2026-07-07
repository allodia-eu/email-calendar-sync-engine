//! Offline tests for the provider's `submit_over` SMTP dispatch and Sent-copy
//! filing, driven over mock streams.
//!
//! Sibling of `provider_tests.rs` (kept separate so that file stays at its line
//! limit).

use engine_core::{
    ids::{MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::Draft;

use super::ImapProvider;
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

/// Builds a provider bound to INBOX over a mock that has greeted and accepted login.
async fn connected_provider(server: Vec<u8>) -> ImapProvider<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap())
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
