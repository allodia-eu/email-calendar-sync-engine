//! Offline tests for SMTP message assembly (`assemble_message`): headers, RFC
//! 2047 encoding, CRLF hardening, and body normalization.
//!
//! Sibling of `smtp_tests.rs` (kept separate so that file stays at its line limit).

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use time::{OffsetDateTime, macros::datetime};

use super::*;

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

#[test]
fn assemble_message_sets_message_id_date_and_crlf_headers() {
    let message = assembled(&draft(&["bob@test.local"], "hello"));
    let text = String::from_utf8(message).unwrap();
    assert!(text.contains("Message-ID: <smtp-test@host>\r\n"));
    assert!(text.contains("From: alice@test.local\r\n"));
    assert!(text.contains("To: bob@test.local\r\n"));
    assert!(text.contains("Subject: Subject line\r\n"));
    // A Date header is generated (RFC 5322 §3.6 requires it).
    assert!(
        text.contains("Date: Sat, 20 Jun 2026 12:00:00 +0000\r\n"),
        "{text}"
    );
    // A blank line separates headers from the body, which is CRLF-terminated.
    assert!(text.contains("\r\n\r\nhello\r\n"));
}

#[test]
fn assemble_message_rejects_header_injection_via_crlf() {
    // A subject carrying CRLF must be rejected, not interpolated — otherwise it
    // injects an arbitrary header (here a Bcc).
    let mut poisoned = draft(&["bob@test.local"], "body");
    poisoned.subject = "Hi\r\nBcc: victim@evil.example".to_owned();
    let err = assemble_message(&poisoned, fixed_date()).unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );

    // A Message-ID and an address with CRLF are rejected the same way.
    let mut bad_addr = draft(&["bob@test.local"], "body");
    bad_addr.from = EmailAddress::new("a@b.com>\r\nRCPT TO:<attacker@evil.example");
    assert!(assemble_message(&bad_addr, fixed_date()).is_err());
}

#[test]
fn assemble_message_encodes_non_ascii_subject_and_display_names() {
    let mut d = draft(&["bob@test.local"], "body");
    d.subject = "Réunion ☕".to_owned();
    d.from = EmailAddress::named("Café Owner", "alice@test.local");
    d.to = vec![EmailAddress::named("Bób", "bob@test.local")];
    let text = String::from_utf8(assembled(&d)).unwrap();
    // No raw 8-bit bytes leak into the headers; the non-ASCII subject/name become
    // RFC 2047 encoded-words, the ASCII name is quoted.
    assert!(text.is_ascii(), "headers must stay 7-bit clean: {text}");
    assert!(text.contains("Subject: =?UTF-8?B?"), "{text}");
    assert!(text.contains("From: \"Café Owner\"") || text.contains("From: =?UTF-8?B?"));
    assert!(text.contains("<bob@test.local>"));
}

#[test]
fn assemble_message_normalizes_a_bare_cr_in_the_body() {
    // A lone CR (legacy-Mac line break) must not reach the wire as a bare CR.
    let message = assembled(&draft(&["bob@test.local"], "a\rb"));
    let body = &message[message.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4..];
    assert_eq!(body, b"a\r\nb\r\n");
}
