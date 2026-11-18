//! The sender's importance, read off the metadata fetch's header section.

use engine_core::{ids::MailboxId, mail::Importance};

use super::message_from_fetch;
use crate::parse::parse_fetch;

/// One metadata row whose header section is `section`, delivered as a literal like a server's.
fn importance_of(section: &str) -> Importance {
    let mut bytes =
        br#"1 FETCH (UID 1 ENVELOPE (NIL "s" NIL NIL NIL NIL NIL NIL NIL "<m@h>") "#.to_vec();
    bytes.extend_from_slice(
        format!(
            "BODY[HEADER.FIELDS (REFERENCES IMPORTANCE X-PRIORITY CONTENT-TYPE \
             CONTENT-DISPOSITION)] {{{}}}\r\n{section})",
            section.len()
        )
        .as_bytes(),
    );
    let rows = parse_fetch(&[bytes]).unwrap();
    message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1)
        .envelope
        .importance
}

#[test]
fn importance_is_read_from_its_own_field() {
    let section = "References: <a@x>\r\nImportance: High\r\nContent-Type: text/plain\r\n\r\n";
    assert_eq!(importance_of(section), Importance::High);
}

#[test]
fn a_folded_importance_is_unfolded() {
    let section = "Importance:\r\n low\r\n\r\n";
    assert_eq!(importance_of(section), Importance::Low);
}

#[test]
fn x_priority_stands_in_when_importance_is_absent() {
    let section = "X-Priority: 1 (Highest)\r\nContent-Type: text/plain\r\n\r\n";
    assert_eq!(importance_of(section), Importance::High);
}

#[test]
fn a_section_without_either_is_normal() {
    let section = "Content-Type: text/plain\r\n\r\n";
    assert_eq!(importance_of(section), Importance::Normal);
}

/// The three rows Stalwart answered the metadata fetch with
/// (`tests/fixtures/importance_fetch.txt`): `Importance` with `X-Priority` beside it, `X-Priority`
/// alone, and neither.
#[test]
fn the_servers_own_answer_reads_as_each_message_was_sent() {
    // Stored LF like every text file here; the literals' lengths count the CRLF of the wire. The
    // transport hands the parser each row without its untagged `* `.
    // Split where a row starts: a header value may itself end a line with `)` (`1 (Highest)`).
    let rows: Vec<Vec<u8>> = include_str!("../tests/fixtures/importance_fetch.txt")
        .split("\n* ")
        .map(|row| {
            row.trim_start_matches("* ")
                .trim_end_matches('\n')
                .replace('\n', "\r\n")
                .into_bytes()
        })
        .collect();
    let importances: Vec<Importance> = parse_fetch(&rows)
        .unwrap()
        .iter()
        .map(|row| {
            message_from_fetch(row, &MailboxId::try_from("INBOX").unwrap(), 1)
                .envelope
                .importance
        })
        .collect();
    assert_eq!(
        importances,
        [Importance::High, Importance::Low, Importance::Normal]
    );
}
