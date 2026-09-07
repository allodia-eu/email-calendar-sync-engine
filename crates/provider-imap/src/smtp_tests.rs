//! Offline tests for the SMTP submission conversation, over a mock stream.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use engine_rfc5322::assemble_message;
use time::{OffsetDateTime, macros::datetime};

use super::*;
use crate::{
    credentials::Credentials,
    mock::{MockStream, script, written},
};

/// The `SmtpAuth` a TLS submission presents. Taken by reference, so the credential is
/// built by the caller and outlives it.
fn auth(credentials: &Credentials) -> SmtpAuth<'_> {
    SmtpAuth {
        credentials,
        host: "smtp.test.local",
        port: Some(465),
    }
}

fn draft(to: &[&str], body: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new("smtp-test@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        to.iter().map(|t| EmailAddress::new(*t)).collect(),
        "Subject line",
        body,
    )
}

/// A fixed instant so the generated `Date` header is deterministic in tests.
fn fixed_date() -> OffsetDateTime {
    datetime!(2026-06-20 12:00:00 UTC)
}

/// Assembles the message bytes for `draft` at [`fixed_date`], unwrapping (the test
/// drafts are always valid).
fn assembled(draft: &Draft) -> Vec<u8> {
    assemble_message(draft, fixed_date()).unwrap()
}

fn recipients(to: &[&str]) -> Vec<String> {
    to.iter().map(|t| (*t).to_owned()).collect()
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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local", "nope@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
async fn a_malformed_post_data_reply_is_ambiguous_not_a_hard_error() {
    // The message bytes are already sent; a garbled final reply (no 3-digit code)
    // means we cannot tell if it delivered, so it is ambiguous — never a plain
    // (retryable/permanent) error that could double-send or wrongly report failure.
    let server = script(&[
        "220 mail\r\n",
        "250 OK\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 go ahead\r\n",
        "garbled not-a-code\r\n", // post-DATA reply with no parseable code
    ]);
    let (stream, _) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
async fn send_rejects_a_recipient_address_carrying_crlf() {
    // A recipient address with CRLF must be rejected before any command is written,
    // so it cannot inject an SMTP command.
    let (stream, recorded) = MockStream::new(script(&["220 mail\r\n"]));
    let message = assembled(&draft(&["bob@test.local"], "hi"));
    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local>\r\nRCPT TO:<attacker@evil.example"]),
        &message,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
    // Nothing was written — the validation happens before the conversation starts.
    assert!(written(&recorded).is_empty());
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
    let message = assembled(&draft(&["nope@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

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
    let message = assembled(&draft(&["bob@test.local"], "hi"));
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
    let message = assembled(&draft(&["bob@test.local"], "hi"));
    let credentials = Credentials::password("alice@test.local", "s3cret");

    let result = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(auth(&credentials)),
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
    let message = assembled(&draft(&["bob@test.local"], "hi"));

    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(auth(&Credentials::password("alice@test.local", "wrong"))),
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
    let message = assembled(&draft(&["bob@test.local"], "hi"));
    let err = send(
        stream,
        "test.local",
        "alice@test.local",
        &recipients(&["bob@test.local"]),
        &message,
        Some(auth(&Credentials::password("user", "pass"))),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}

#[tokio::test]
async fn an_endless_multiline_reply_is_capped() {
    // A server that never terminates its multiline reply (`NNN-...` forever) must be
    // bounded, not hang the submission or grow the joined text without limit.
    let server = "220-still going\r\n".repeat(300).into_bytes();
    let (stream, _) = MockStream::new(server);
    let message = assembled(&draft(&["bob@test.local"], "hi"));
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
