//! Tests for [`crate::parse_body`].

use super::parse_fetch_body;

/// Builds the `Vec<Vec<u8>>` shape the transport hands the parsers (each entry is
/// one untagged response body, `* ` already stripped).
fn lines(strs: &[&str]) -> Vec<Vec<u8>> {
    strs.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn fetch_body_extracts_the_literal_payload_for_its_uid() {
    // The transport inlines the `{n}` literal: the untagged entry is the framing,
    // the n raw bytes, then the closing `)`.
    let body = "Subject: x\r\n\r\nbody bytes";
    let line = format!("3 FETCH (UID 7 BODY[] {{{}}}\r\n{body})", body.len());
    let raw = parse_fetch_body(&lines(&[&line]), 7).unwrap();
    assert_eq!(raw, body.as_bytes());
}

#[test]
fn fetch_body_accepts_a_uid_that_follows_the_literal() {
    // `UID FETCH n (BODY.PEEK[])` obliges the server to return a UID nobody asked for, and
    // RFC 9051 orders no FETCH item, so a server that appends rather than prepends puts it
    // after the payload. Observed from Proton Mail Bridge, whose reply frames as
    // `* 1449 FETCH (BODY[] {n}\r\n<payload> UID 1449)`; reading only the prefix finds no UID
    // on any message and reports every body as expunged.
    let body = "Subject: x\r\n\r\nbody bytes";
    let line = format!("1449 FETCH (BODY[] {{{}}}\r\n{body} UID 1449)", body.len());
    let raw = parse_fetch_body(&lines(&[&line]), 1449).unwrap();
    assert_eq!(raw, body.as_bytes());
}

#[test]
fn fetch_body_is_not_named_by_a_uid_inside_the_payload() {
    // Framing on either side of the literal names the UID; the payload between them never
    // does, so a message whose own bytes mention one cannot pass itself off as its framing.
    let body = "Subject: x\r\n\r\nUID 7 appears in this body";
    let line = format!("3 FETCH (BODY[] {{{}}}\r\n{body})", body.len());
    assert!(parse_fetch_body(&lines(&[&line]), 7).is_none());
}

#[test]
fn fetch_body_ignores_a_line_for_another_uid() {
    // A piggybacked FETCH for a different UID (and the `UID 70` red herring) must not
    // be mistaken for our UID 7's body.
    let body = "wrong message";
    let line = format!("3 FETCH (UID 70 BODY[] {{{}}}\r\n{body})", body.len());
    assert!(parse_fetch_body(&lines(&[&line]), 7).is_none());
}

#[test]
fn fetch_body_without_a_literal_for_the_uid_is_none() {
    // No `BODY[]` section for the UID (expunged / not returned) → None, never a panic.
    assert!(parse_fetch_body(&lines(&["3 FETCH (UID 7 FLAGS (\\Seen))"]), 7).is_none());
    assert!(parse_fetch_body(&lines(&[]), 7).is_none());
}

#[test]
fn fetch_body_truncated_below_announced_length_is_none() {
    // The server announced 999 bytes but sent far fewer: reject rather than read out
    // of bounds.
    let line = "3 FETCH (UID 7 BODY[] {999}\r\nonly a few bytes)";
    assert!(parse_fetch_body(&lines(&[line]), 7).is_none());
}

#[test]
fn fetch_body_with_malformed_framing_is_none() {
    // `BODY[]` present but no `{n}` length framing follows.
    let line = "3 FETCH (UID 7 BODY[] NIL)";
    assert!(parse_fetch_body(&lines(&[line]), 7).is_none());
}
