//! Offline tests for the IMAP response parsers, including adversarial input.

use super::*;

/// Builds the `Vec<Vec<u8>>` shape the transport hands the parsers (each entry is
/// one untagged response body, `* ` already stripped).
fn lines(strs: &[&str]) -> Vec<Vec<u8>> {
    strs.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn select_extracts_uidvalidity_uidnext_and_exists() {
    let data = parse_select(&lines(&[
        "8 EXISTS",
        "0 RECENT",
        "OK [UIDVALIDITY 1234567890] UIDs valid",
        "OK [UIDNEXT 10] Predicted next UID",
        r"FLAGS (\Answered \Flagged \Deleted \Seen \Draft)",
    ]))
    .unwrap();
    assert_eq!(data.uid_validity, 1_234_567_890);
    assert_eq!(data.uid_next, Some(10));
    assert_eq!(data.exists, 8);
}

#[test]
fn select_without_uidvalidity_is_a_protocol_error() {
    // Identity cannot be keyed without UIDVALIDITY, so this must fail, not default.
    let err = parse_select(&lines(&["3 EXISTS", "OK [UIDNEXT 4] no validity here"])).unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}

#[test]
fn list_parses_attributes_delimiter_and_name() {
    let rows = parse_list(&lines(&[
        r#"LIST (\HasNoChildren) "/" "INBOX""#,
        r#"LIST (\HasNoChildren \Sent) "/" "Sent""#,
        r#"LIST (\HasNoChildren) "/" "Archive""#,
    ]))
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].name, "INBOX");
    assert_eq!(rows[0].delimiter.as_deref(), Some("/"));
    assert_eq!(rows[1].name, "Sent");
    assert!(rows[1].attributes.iter().any(|a| a == r"\Sent"));
    assert_eq!(rows[2].name, "Archive");
    assert!(rows[2].attributes.iter().all(|a| a != r"\Sent"));
}

#[test]
fn list_unescapes_a_quoted_name() {
    let rows = parse_list(&lines(&[r#"LIST () "/" "weird\"name""#])).unwrap();
    assert_eq!(rows[0].name, r#"weird"name"#);
    // A NIL delimiter (flat namespace) is preserved as None.
    let flat = parse_list(&lines(&[r#"LIST () NIL "Flat""#])).unwrap();
    assert_eq!(flat[0].delimiter, None);
}

#[test]
fn fetch_parses_uid_flags_internaldate_size_and_envelope() {
    let line = concat!(
        r#"1 FETCH (UID 1 FLAGS (\Seen \Flagged harness) "#,
        r#"INTERNALDATE "18-Mar-2026 10:00:00 +0000" RFC822.SIZE 2048 "#,
        r#"ENVELOPE ("Wed, 18 Mar 2026 10:00:00 +0000" "Harness baseline message" "#,
        r#"(("Alice Tester" NIL "alice" "test.local")) "#,
        r#"(("Alice Tester" NIL "alice" "test.local")) NIL "#,
        r#"(("Bob Tester" NIL "bob" "test.local")) NIL NIL NIL "#,
        r#""<baseline-0001@test.local>"))"#,
    );
    let rows = parse_fetch(&lines(&[line])).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.uid, 1);
    assert_eq!(row.flags, vec![r"\Seen", r"\Flagged", "harness"]);
    assert_eq!(
        row.internal_date.as_deref(),
        Some("18-Mar-2026 10:00:00 +0000")
    );
    assert_eq!(row.size, Some(2048));

    let env = row.envelope.as_ref().unwrap();
    assert_eq!(env.subject.as_deref(), Some("Harness baseline message"));
    assert_eq!(env.from[0].name.as_deref(), Some("Alice Tester"));
    assert_eq!(env.from[0].mailbox.as_deref(), Some("alice"));
    assert_eq!(env.from[0].host.as_deref(), Some("test.local"));
    assert_eq!(env.to[0].mailbox.as_deref(), Some("bob"));
    assert!(env.cc.is_empty());
    assert_eq!(
        env.message_id.as_deref(),
        Some("<baseline-0001@test.local>")
    );
}

#[test]
fn fetch_skips_unsolicited_flag_only_rows() {
    // A flag update with no UID is not a usable mail object; skip it, don't error.
    let rows = parse_fetch(&lines(&[r"2 FETCH (FLAGS (\Seen))"])).unwrap();
    assert!(rows.is_empty());
}

#[test]
fn fetch_reads_a_literal_string() {
    // ENVELOPE subject delivered as a `{7}` literal the transport inlined.
    let line =
        b"3 FETCH (UID 9 ENVELOPE (NIL {7}\r\nSubject NIL NIL NIL NIL NIL NIL NIL NIL))".to_vec();
    let rows = parse_fetch(&[line]).unwrap();
    assert_eq!(rows[0].uid, 9);
    assert_eq!(
        rows[0].envelope.as_ref().unwrap().subject.as_deref(),
        Some("Subject")
    );
}

#[test]
fn parsers_reject_malformed_input_without_panicking() {
    // Each adversarial body must return an error (or empty), never panic.
    // A stray `)` and deep nesting are the cases that must terminate (no infinite
    // loop, no stack overflow), not just "not panic".
    let deep_nest = b"1 FETCH "
        .iter()
        .copied()
        .chain(std::iter::repeat_n(b'(', 5000))
        .collect();
    let hostile: Vec<Vec<u8>> = vec![
        b"((((".to_vec(),
        b"))))".to_vec(),
        b") stray closer".to_vec(),
        b"\"unterminated".to_vec(),
        b"{999}\r\nshort".to_vec(),
        b"{notanumber}".to_vec(),
        b"\"bad\\zescape\"".to_vec(),
        b"\xff\xfe\x00 )(  garbage".to_vec(),
        b"1 FETCH (UID)".to_vec(),
        b"1 FETCH".to_vec(),
        b"".to_vec(),
        deep_nest,
    ];
    for case in &hostile {
        let batch = vec![case.clone()];
        // None of these should panic, hang, or overflow; Err or empty is fine.
        let _ = parse_fetch(&batch);
        let _ = parse_list(&batch);
        let _ = parse_select(&batch);
    }
    // A structurally broken list is a hard protocol error for FETCH.
    assert!(parse_fetch(&[b"1 FETCH ((((".to_vec()]).is_err());
    // Depth-bounded: deep nesting errors rather than overflowing the stack.
    let too_deep: Vec<u8> = std::iter::repeat_n(b'(', 5000).collect();
    assert!(parse_list(&[too_deep]).is_err());
}

#[test]
fn envelope_handles_all_nil_addresses() {
    // Every address slot NIL — no addresses, no message-id, no panic.
    let line = "4 FETCH (UID 12 ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL))";
    let rows = parse_fetch(&lines(&[line])).unwrap();
    let env = rows[0].envelope.as_ref().unwrap();
    assert!(env.from.is_empty() && env.to.is_empty());
    assert_eq!(env.subject, None);
    assert_eq!(env.message_id, None);
}
