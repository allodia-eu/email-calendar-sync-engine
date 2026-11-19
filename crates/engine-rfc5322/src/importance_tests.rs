//! Tests for the importance headers (`Importance`, RFC 2156 §5.3, and `X-Priority`).

use engine_core::{
    ids::MessageIdHeader,
    mail::{EmailAddress, Importance},
};
use engine_provider::Draft;
use time::macros::datetime;

use super::*;

fn draft(importance: Importance) -> Draft {
    Draft::new(
        MessageIdHeader::new("m@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject line",
        "body",
    )
    .with_importance(importance)
}

/// The header block of every assembly, so a header that reaches only one of them shows up.
fn header_blocks(importance: Importance) -> Vec<String> {
    let at = datetime!(2026-06-20 12:00:00 UTC);
    let draft = draft(importance);
    [
        assemble_message(&draft, at),
        assemble_filed_message(&draft, at),
        assemble_draft_message(&draft, at),
    ]
    .into_iter()
    .map(|bytes| {
        let text = String::from_utf8(bytes.unwrap()).unwrap();
        text.split("\r\n\r\n").next().unwrap().to_owned()
    })
    .collect()
}

#[test]
fn high_importance_is_written_as_both_headers_in_every_assembly() {
    for headers in header_blocks(Importance::High) {
        assert!(headers.contains("\r\nImportance: high\r\n"), "{headers}");
        assert!(
            headers.contains("\r\nX-Priority: 1 (Highest)\r\n"),
            "{headers}"
        );
    }
}

#[test]
fn low_importance_is_written_as_both_headers() {
    for headers in header_blocks(Importance::Low) {
        assert!(headers.contains("\r\nImportance: low\r\n"), "{headers}");
        assert!(
            headers.contains("\r\nX-Priority: 5 (Lowest)\r\n"),
            "{headers}"
        );
    }
}

#[test]
fn normal_importance_writes_neither_header() {
    for headers in header_blocks(Importance::Normal) {
        assert!(!headers.contains("Importance:"), "{headers}");
        assert!(!headers.contains("X-Priority:"), "{headers}");
    }
}
