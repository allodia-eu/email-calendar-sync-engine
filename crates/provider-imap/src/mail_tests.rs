//! Offline tests for IMAP → domain normalization.

use super::*;
use crate::parse::parse_fetch;
use engine_core::mail::SystemKeyword;

#[test]
fn synthesized_key_is_stable_and_distinguishes_copies() {
    let inbox_5 = message_key("INBOX", 1, 5);
    // Stable: the same triple always yields the same key (idempotent re-sync).
    assert_eq!(inbox_5, message_key("INBOX", 1, 5));
    // A copy in another folder is a DISTINCT object (different mailbox component) —
    // the IMAP contrast to JMAP's one multi-membership object.
    assert_ne!(inbox_5, message_key("Archive", 1, 5));
    // A UIDVALIDITY change invalidates every old key.
    assert_ne!(inbox_5, message_key("INBOX", 2, 5));
    // Different UIDs are different objects.
    assert_ne!(inbox_5, message_key("INBOX", 1, 6));
    assert_eq!(inbox_5.as_str(), "imap:v1:u5@INBOX");
}

#[test]
fn flags_map_to_keywords_excluding_deleted_and_recent() {
    let flags = [
        r"\Seen".to_owned(),
        r"\Flagged".to_owned(),
        r"\Answered".to_owned(),
        r"\Deleted".to_owned(),
        r"\Recent".to_owned(),
        "harness".to_owned(),
    ];
    let keywords = flags_to_keywords(&flags);
    assert!(keywords.contains(&Keyword::system(SystemKeyword::Seen)));
    assert!(keywords.contains(&Keyword::system(SystemKeyword::Flagged)));
    assert!(keywords.contains(&Keyword::system(SystemKeyword::Answered)));
    assert!(keywords.contains(&Keyword::new("harness").unwrap()));
    // \Deleted and \Recent are not keywords.
    assert!(!keywords.iter().any(|k| k.as_str().contains("deleted")));
    assert!(!keywords.iter().any(|k| k.as_str().contains("recent")));
}

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

#[test]
fn message_from_fetch_builds_a_full_object() {
    let line = concat!(
        r#"1 FETCH (UID 1 FLAGS (\Seen \Flagged harness) "#,
        r#"INTERNALDATE "18-Mar-2026 10:00:00 +0000" RFC822.SIZE 2048 "#,
        r#"ENVELOPE ("Wed, 18 Mar 2026 10:00:00 +0000" "Harness baseline message" "#,
        r#"(("Alice Tester" NIL "alice" "test.local")) "#,
        r#"(("Alice Tester" NIL "alice" "test.local")) NIL "#,
        r#"(("Bob Tester" NIL "bob" "test.local")) NIL NIL NIL "#,
        r#""<baseline-0001@test.local>"))"#,
    );
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let mailbox = MailboxId::try_from("INBOX").unwrap();
    let message = message_from_fetch(&rows[0], &mailbox, 1_234_567_890);

    assert_eq!(message.id.as_str(), "imap:v1234567890:u1@INBOX");
    assert_eq!(message.mailboxes.len().get(), 1);
    assert!(message.mailboxes.contains(&mailbox));
    assert!(message.has_system_keyword(SystemKeyword::Seen));
    assert!(message.has_system_keyword(SystemKeyword::Flagged));
    assert!(message.has_keyword(&Keyword::new("harness").unwrap()));
    assert_eq!(message.size, Some(2048));
    assert_eq!(
        message.received_at.unwrap().to_string(),
        "2026-03-18T10:00:00Z"
    );
    assert_eq!(
        message.envelope.subject.as_deref(),
        Some("Harness baseline message")
    );
    assert_eq!(message.envelope.from[0].email, "alice@test.local");
    assert_eq!(
        message.envelope.from[0].name.as_deref(),
        Some("Alice Tester")
    );
    assert_eq!(message.envelope.to[0].email, "bob@test.local");
    assert_eq!(
        message.envelope.message_id[0].as_str(),
        "baseline-0001@test.local"
    );
    // A message with no synced raw body keeps blob_id None (Tier-1 metadata).
    assert!(message.blob_id.is_none());
}

#[test]
fn references_header_threads_through_to_the_envelope() {
    // The References header rides BODY[HEADER.FIELDS (REFERENCES)]; the chain of
    // ids surfaces on Message.envelope.references for local threading. The header
    // value is delivered as a `{n}` literal so its real trailing CRLFs are present
    // (the quoted-string parser would reject raw CR/LF).
    let mut bytes =
        br#"1 FETCH (UID 1 ENVELOPE (NIL "s" NIL NIL NIL NIL NIL NIL "<r@h>" "<m@h>") "#.to_vec();
    bytes.extend_from_slice(b"BODY[HEADER.FIELDS (REFERENCES)] {27}\r\n");
    bytes.extend_from_slice(b"References: <a@x> <b@y>\r\n\r\n");
    bytes.push(b')');

    let rows = parse_fetch(&[bytes]).unwrap();
    let message = message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1);
    let refs = &message.envelope.references;
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].as_str(), "a@x");
    assert_eq!(refs[1].as_str(), "b@y");
    // In-Reply-To (an ENVELOPE field) is unaffected and still populated.
    assert_eq!(message.envelope.in_reply_to[0].as_str(), "r@h");
}

#[test]
fn an_empty_references_header_yields_no_ids() {
    // A message with no References: the echoed value is empty, so the field name
    // must not be mistaken for an id (the bare-value fallback's trap).
    let line = concat!(
        r#"1 FETCH (UID 1 ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) "#,
        r#"BODY[HEADER.FIELDS (REFERENCES)] "References: ")"#,
    );
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let message = message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1);
    assert!(message.envelope.references.is_empty());
}

#[test]
fn encoded_word_subjects_are_decoded_through_normalization() {
    // A non-ASCII subject arrives RFC 2047-encoded in the ENVELOPE; it is decoded.
    let line = "1 FETCH (UID 1 ENVELOPE (NIL \"=?UTF-8?Q?Caf=C3=A9?=\" \
                NIL NIL NIL NIL NIL NIL NIL NIL))";
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let message = message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1);
    assert_eq!(message.envelope.subject.as_deref(), Some("Café"));
}

#[test]
fn mailbox_from_list_maps_inbox_special_use_and_roleless() {
    let rows = crate::parse::parse_list(&[
        br#"LIST (\HasNoChildren) "/" "INBOX""#.to_vec(),
        br#"LIST (\HasNoChildren \Sent) "/" "Sent""#.to_vec(),
        br#"LIST (\HasNoChildren) "/" "Archive""#.to_vec(),
    ])
    .unwrap();
    let mailboxes: Vec<_> = rows.iter().filter_map(mailbox_from_list).collect();

    let inbox = mailboxes.iter().find(|m| m.name == "INBOX").unwrap();
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    let sent = mailboxes.iter().find(|m| m.name == "Sent").unwrap();
    assert_eq!(sent.role, Some(MailboxRole::Sent));
    // The custom Archive folder has no SPECIAL-USE attribute → roleless.
    let archive = mailboxes.iter().find(|m| m.name == "Archive").unwrap();
    assert_eq!(archive.role, None);
}

#[test]
fn hierarchy_parent_is_derived_from_the_delimiter() {
    let rows = crate::parse::parse_list(&[br#"LIST () "/" "Work/Clients""#.to_vec()]).unwrap();
    let mailbox = mailbox_from_list(&rows[0]).unwrap();
    assert_eq!(mailbox.parent.as_ref().unwrap().as_str(), "Work");
}

#[test]
fn message_ids_handle_brackets_and_multiples() {
    assert_eq!(extract_message_ids("<one@host>")[0].as_str(), "one@host");
    let many = extract_message_ids("<a@host> <b@host>");
    assert_eq!(many.len(), 2);
    assert_eq!(many[1].as_str(), "b@host");
    // A bare (bracket-less) value is accepted as a single id.
    assert_eq!(extract_message_ids("bare@host")[0].as_str(), "bare@host");
    // Empty/garbage yields nothing, never a panic.
    assert!(extract_message_ids("<>").is_empty());
}
