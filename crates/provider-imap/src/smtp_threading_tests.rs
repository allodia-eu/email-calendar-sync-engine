//! Offline tests for the SMTP threading headers (`In-Reply-To` / `References`).
//!
//! Sibling of `smtp_tests.rs` (kept separate so that file stays at its line limit).

use super::*;
use engine_core::ids::MessageIdHeader;
use engine_core::mail::EmailAddress;
use engine_provider::Draft;
use time::macros::datetime;

fn mid(value: &str) -> MessageIdHeader {
    MessageIdHeader::new(value).unwrap()
}

/// A reply draft (no threading linkage yet); add it with [`Draft::in_reply_to`].
fn reply_draft() -> Draft {
    Draft::new(
        mid("reply@host"),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Re: Subject line",
        "thanks",
    )
}

/// Assembles the message at a fixed instant and returns it as text.
fn assemble(draft: &Draft) -> String {
    let bytes = assemble_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn assemble_message_emits_in_reply_to_and_references_when_present() {
    let draft = reply_draft().in_reply_to(
        mid("parent@host"),
        vec![mid("root@host"), mid("parent@host")],
    );
    let text = assemble(&draft);
    // The parent's Message-ID is the In-Reply-To, angle-bracketed like Message-ID.
    assert!(text.contains("In-Reply-To: <parent@host>\r\n"), "{text}");
    // The References chain is space-separated, each id angle-bracketed (§3.6.4).
    assert!(
        text.contains("References: <root@host> <parent@host>\r\n"),
        "{text}"
    );
}

#[test]
fn assemble_message_omits_both_threading_headers_for_an_original() {
    // A draft with no in_reply_to and an empty references chain emits NEITHER header.
    let text = assemble(&reply_draft());
    assert!(!text.contains("In-Reply-To:"), "{text}");
    assert!(!text.contains("References:"), "{text}");
}

#[test]
fn assemble_message_emits_only_in_reply_to_when_references_is_empty() {
    let mut draft = reply_draft();
    draft.in_reply_to = Some(mid("parent@host"));
    let text = assemble(&draft);
    assert!(text.contains("In-Reply-To: <parent@host>\r\n"), "{text}");
    assert!(!text.contains("References:"), "{text}");
}

#[test]
fn assemble_message_rejects_a_threading_id_carrying_crlf() {
    // MessageIdHeader::new does not screen control characters, so the header-injection
    // guard must be reject_control at assembly time. An In-Reply-To id with CRLF...
    let mut bad_parent = reply_draft();
    bad_parent.in_reply_to = Some(mid("evil@host>\r\nBcc: victim@evil.example"));
    let err = assemble_message(&bad_parent, datetime!(2026-06-20 12:00:00 UTC)).unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );

    // ...and a References id with CRLF are both rejected the same way.
    let mut bad_ref = reply_draft();
    bad_ref.references = vec![
        mid("ok@host"),
        mid("evil@host>\r\nBcc: victim@evil.example"),
    ];
    assert!(assemble_message(&bad_ref, datetime!(2026-06-20 12:00:00 UTC)).is_err());
}
