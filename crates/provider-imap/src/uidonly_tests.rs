//! Reading what a server sends once UIDONLY (RFC 9586) is enabled: every `FETCH` response is a
//! `UIDFETCH`, numbered by UID rather than by sequence number. The lines are RFC 9586's own
//! examples, with a body literal added in the same shape.

use crate::{
    parse::parse_fetch,
    parse_body::{parse_any_fetch_body, parse_fetch_body},
};

#[test]
fn a_uidfetch_row_is_a_row_numbered_by_its_uid() {
    let rows = parse_fetch(&[
        b"25996 UIDFETCH (FLAGS (\\Seen))".to_vec(),
        b"25997 UIDFETCH (FLAGS (\\Flagged \\Answered))".to_vec(),
    ])
    .unwrap();
    let uids: Vec<u32> = rows.iter().map(|row| row.uid).collect();
    assert_eq!(uids, [25996, 25997]);
    assert_eq!(rows[1].flags, ["\\Flagged", "\\Answered"]);
}

#[test]
fn a_uid_item_beside_it_agrees_and_is_read_the_same() {
    // The server may also return the `UID` item if the client asked for it.
    let rows = parse_fetch(&[b"7 UIDFETCH (UID 7 FLAGS ())".to_vec()]).unwrap();
    assert_eq!(rows[0].uid, 7);
}

#[test]
fn a_body_in_uid_mode_belongs_to_the_uid_that_numbers_it() {
    let line = b"* 5 UIDFETCH (BODY[] {5}\r\nHello)".to_vec();
    assert_eq!(
        parse_fetch_body(&[line[2..].to_vec()], 5),
        Some(b"Hello".to_vec())
    );
    assert_eq!(
        parse_any_fetch_body(&line[2..]),
        Some((5, b"Hello".to_vec()))
    );
}
