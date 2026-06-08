//! Offline tests for the SMTP submission conversation, over a mock stream.

use super::*;
use crate::mock::{MockStream, script, written};
use engine_core::ids::MessageIdHeader;
use engine_core::mail::EmailAddress;
use engine_provider::Draft;

fn draft(to: &[&str], body: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new("smtp-test@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        to.iter().map(|t| EmailAddress::new(*t)).collect(),
        "Subject line",
        body,
    )
}

fn recipients(to: &[&str]) -> Vec<String> {
    to.iter().map(|t| (*t).to_owned()).collect()
}

#[test]
fn assemble_message_sets_message_id_and_crlf_headers() {
    let message = assemble_message(&draft(&["bob@test.local"], "hello"));
    let text = String::from_utf8(message).unwrap();
    assert!(text.contains("Message-ID: <smtp-test@host>\r\n"));
    assert!(text.contains("From: alice@test.local\r\n"));
    assert!(text.contains("To: bob@test.local\r\n"));
    assert!(text.contains("Subject: Subject line\r\n"));
    // A blank line separates headers from the body, which is CRLF-terminated.
    assert!(text.contains("\r\n\r\nhello\r\n"));
}

#[tokio::test]
async fn send_delivers_on_a_clean_250() {
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    assert_eq!(result.recipients.len(), 1);
    assert!(result.recipients[0].accepted);

    let sent = written(&recorded);
    assert!(sent.contains("EHLO test.local\r\n"));
    assert!(sent.contains("MAIL FROM:<alice@test.local>\r\n"));
    assert!(sent.contains("RCPT TO:<bob@test.local>\r\n"));
    assert!(sent.contains("DATA\r\n"));
    assert!(sent.contains("Message-ID: <smtp-test@host>\r\n"));
    assert!(
        sent.contains("\r\n.\r\n"),
        "the message terminates with <CRLF>.<CRLF>"
    );
    assert!(sent.contains("QUIT\r\n"));
}

#[tokio::test]
async fn send_records_per_recipient_acceptance_and_rejection() {
    // bob is accepted (250), the bogus recipient is rejected (550) — both
    // represented — and the message still goes to the accepted one.
    let server = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",              // RCPT bob
        "550 5.1.2 no such mailbox\r\n", // RCPT bogus
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local", "nope@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local", "nope@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    assert!(result.recipients[0].accepted);
    assert!(!result.recipients[1].accepted);
    assert!(result.recipients[1].response.contains("no such mailbox"));
}

#[tokio::test]
async fn a_lost_post_data_acknowledgement_is_ambiguous() {
    // The server accepts through DATA, then the connection drops before the final
    // reply: the message may or may not have delivered → never blind-retry.
    let server = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        // no post-DATA reply: EOF
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    assert!(matches!(result.disposition, Disposition::Ambiguous(_)));
}

#[tokio::test]
async fn all_recipients_rejected_skips_data_and_is_permanent() {
    let server = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "550 5.1.2 no such mailbox\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assemble_message(&draft(&["nope@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["nope@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    assert!(matches!(
        result.disposition,
        Disposition::RejectedPermanent(_)
    ));
    assert!(!result.recipients[0].accepted);
    // No DATA is sent when nobody accepted.
    assert!(!written(&recorded).contains("DATA\r\n"));
}

#[tokio::test]
async fn a_mail_from_rejection_is_classified_without_recipients() {
    let server = script(&["220 mail\r\n", "250 OK\r\n", "451 4.7.1 try later\r\n"]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    // 4xx is transient (retryable), and no recipient phase ran.
    assert!(matches!(
        result.disposition,
        Disposition::RejectedTransient(_)
    ));
    assert!(result.recipients.is_empty());
}

#[tokio::test]
async fn send_falls_back_to_helo_when_ehlo_is_refused() {
    let server = script(&[
        "220 mail\r\n",
        "502 EHLO not supported\r\n", // EHLO refused
        "250 OK\r\n",                 // HELO accepted
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    assert!(written(&recorded).contains("HELO test.local\r\n"));
}

#[tokio::test]
async fn data_refused_is_a_rejection() {
    let server = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "554 5.7.1 no DATA for you\r\n", // DATA refused (not 354)
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        result.disposition,
        Disposition::RejectedPermanent(_)
    ));
}

#[tokio::test]
async fn a_bad_greeting_or_malformed_reply_errors() {
    // A non-220 greeting is a protocol error, not a delivery outcome.
    let (stream, _) = MockStream::new(script(&["554 go away\r\n"]));
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));
    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );

    // A reply without a 3-digit code is malformed.
    let (stream, _) = MockStream::new(script(&["xx not a code\r\n"]));
    assert!(
        send(
            stream,
            "test.local",
            "alice@test.local",
            &recipients(&["bob@test.local"]),
            &message,
            None,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn send_authenticates_with_auth_plain_over_the_stream() {
    let server = script(&[
        "220 mail ESMTP\r\n",
        "250-mail\r\n250 AUTH PLAIN\r\n",
        "235 2.7.0 authenticated\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued\r\n",
        "221 bye\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("alice@test.local", "s3cret")),
    )
    .await
    .unwrap();

    assert_eq!(result.disposition, Disposition::Delivered);
    let sent = written(&recorded);
    assert!(sent.contains("AUTH PLAIN "), "{sent}");
    // The password is base64 in the SASL token, never in the clear.
    assert!(
        !sent.contains("s3cret"),
        "credentials leaked in the clear: {sent}"
    );
}

#[tokio::test]
async fn an_auth_rejection_is_an_authentication_error() {
    let server = script(&[
        "220 mail\r\n",
        "250 AUTH PLAIN\r\n",
        "535 5.7.8 bad credentials\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));

    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("alice@test.local", "wrong")),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Authentication
    );
}

#[tokio::test]
async fn auth_without_esmtp_is_a_protocol_error() {
    // EHLO is refused (HELO-only), so AUTH cannot run.
    let server = script(&["220 mail\r\n", "502 no EHLO\r\n", "250 OK\r\n"]);
    let (stream, _) = MockStream::new(server);
    let message = assemble_message(&draft(&["bob@test.local"], "hi"));
    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(("user", "pass")),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}

#[test]
fn base64_encodes_the_rfc_4648_vectors() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
}

#[test]
fn dot_stuffing_escapes_leading_dots() {
    let stuffed = dot_stuff(b".hidden\r\nnormal\r\n..already\r\n");
    let text = String::from_utf8(stuffed).unwrap();
    // A line beginning with `.` gets a second `.`; others are untouched.
    assert!(text.starts_with("..hidden\r\n"));
    assert!(text.contains("\r\nnormal\r\n"));
    assert!(text.contains("\r\n...already\r\n"));
}
