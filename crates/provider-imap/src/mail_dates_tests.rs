//! A message's two instants: when this server stored it, and when it says it was sent.

use engine_core::ids::MailboxId;

use super::{message_from_fetch, parse_internaldate};
use crate::parse::parse_fetch;

#[test]
fn internaldate_parses_and_applies_the_zone_offset() {
    // UTC stays put.
    let utc = parse_internaldate("18-Mar-2026 10:00:00 +0000").unwrap();
    assert_eq!(utc.to_string(), "2026-03-18T10:00:00Z");
    // A positive offset is subtracted to reach UTC.
    let plus = parse_internaldate("18-Mar-2026 12:00:00 +0200").unwrap();
    assert_eq!(plus.to_string(), "2026-03-18T10:00:00Z");
    // A negative offset across midnight rolls the date.
    let minus = parse_internaldate(" 1-Jan-2026 00:30:00 -0100").unwrap();
    assert_eq!(minus.to_string(), "2026-01-01T01:30:00Z");
    // Garbage yields None, never a panic.
    assert!(parse_internaldate("not a date").is_none());
    assert!(parse_internaldate("32-Xxx-2026 99:99:99 +0000").is_none());
}

/// `Message::sent_at` is the `Date` header's instant, which ENVELOPE carries as its first field
/// (RFC 9051 §7.5.2), as JMAP's `sentAt` is; INTERNALDATE is when this server stored the copy
/// (RFC 9051 §2.3.3), which for a Sent copy filed, imported or moved later is another instant.
#[test]
fn message_from_fetch_reads_when_it_was_sent_from_the_date_header() {
    let line = concat!(
        r#"1 FETCH (UID 1 INTERNALDATE "20-Nov-2026 20:15:44 +0000" "#,
        r#"ENVELOPE ("Thu, 29 Oct 2026 10:00:00 +0100" "Re: Planning" "#,
        r#"(("Alice" NIL "alice" "test.local")) (("Alice" NIL "alice" "test.local")) NIL "#,
        r#"(("Bob" NIL "bob" "test.local")) NIL NIL NIL "<reply-0001@test.local>"))"#,
    );
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let mailbox = MailboxId::try_from("Sent").unwrap();
    let message = message_from_fetch(&rows[0], &mailbox, 1);

    assert_eq!(
        message.received_at.unwrap().to_string(),
        "2026-11-20T20:15:44Z"
    );
    assert_eq!(
        message.sent_at.map(|at| at.to_string()).as_deref(),
        Some("2026-10-29T09:00:00Z"),
        "the Date header, normalised to UTC"
    );
}

/// A `Date` that is absent or does not parse leaves `sent_at` unset and never fails the row.
#[test]
fn a_missing_or_unreadable_date_leaves_sent_at_unset() {
    for date in ["NIL", r#""not a date""#, r#""""#] {
        let line = format!(
            r#"2 FETCH (UID 2 INTERNALDATE "20-Nov-2026 20:15:44 +0000" ENVELOPE ({date} "Re: Planning" NIL NIL NIL NIL NIL NIL NIL NIL))"#
        );
        let rows = parse_fetch(&[line.into_bytes()]).unwrap();
        let message = message_from_fetch(&rows[0], &MailboxId::try_from("Sent").unwrap(), 1);
        assert!(message.sent_at.is_none(), "{date}");
        assert!(message.received_at.is_some(), "{date}");
        assert_eq!(message.envelope.subject.as_deref(), Some("Re: Planning"));
    }
}

/// The rows Stalwart answered the metadata fetch with for messages whose `Date` headers it was
/// handed in several spellings (`tests/fixtures/sent_at_fetch.txt`): each reads as sent when its
/// header says, and as received when the server stored it.
#[test]
fn the_servers_own_answer_reads_each_date_header() {
    // Stored LF like every text file here; the literals' lengths count the CRLF of the wire.
    let rows: Vec<Vec<u8>> = include_str!("../tests/fixtures/sent_at_fetch.txt")
        .split("\n* ")
        .map(|row| {
            row.trim_start_matches("* ")
                .trim_end_matches('\n')
                .replace('\n', "\r\n")
                .into_bytes()
        })
        .collect();
    let read: Vec<(Option<String>, bool)> = parse_fetch(&rows)
        .unwrap()
        .iter()
        .map(|row| {
            let message = message_from_fetch(row, &MailboxId::try_from("INBOX").unwrap(), 1);
            (
                message.sent_at.map(|at| at.to_string()),
                message.received_at.is_some(),
            )
        })
        .collect();
    let sent = |at: &str| (Some(at.to_owned()), true);
    assert_eq!(
        read,
        [
            sent("2026-10-29T09:00:00Z"),
            sent("2026-10-29T15:00:00Z"),
            sent("2026-10-29T10:00:00Z"),
            sent("2026-10-29T10:00:00Z"),
            (None, true),
        ]
    );
}
