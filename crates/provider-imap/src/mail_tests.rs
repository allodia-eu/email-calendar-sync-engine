//! Offline tests for IMAP → domain normalization.

use engine_core::mail::SystemKeyword;

use super::*;
use crate::parse::parse_fetch;

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
    assert!(!message.has_attachment);
    // A message with no synced raw body keeps blob_id None (Tier-1 metadata).
    assert!(message.blob_id.is_none());
}

#[test]
fn message_from_fetch_carries_the_bodystructure_attachment_flag() {
    let line = concat!(
        r#"1 FETCH (UID 1 BODYSTRUCTURE ("APPLICATION" "PDF" ("NAME" "report.pdf") "#,
        r#"NIL NIL "BASE64" 12 NIL ("ATTACHMENT" ("FILENAME" "report.pdf")) NIL))"#,
    );
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let message = message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1);
    assert!(message.has_attachment);
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
fn a_legacy_charset_subject_and_display_name_are_decoded() {
    // Observed on real Japanese mail. `ISO-2022-JP` is 7-bit, so a UTF-8 read of it
    // yields no replacement character to notice — the escape sequences simply arrive as
    // text (`$B...(B`) in the list row and the notification.
    let line = "1 FETCH (UID 1 ENVELOPE (NIL \
                \"=?iso-2022-jp?b?GyRCPzckNyQkPnBKcyRyJCpDTiRpJDskNyReJDkbKEI=?=\" \
                ((\"=?iso-2022-jp?b?GyRCJSslOSU/JV4hPCU1JV0hPCVIGyhC?=\" NIL \
                \"support\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL))";
    let rows = parse_fetch(&[line.as_bytes().to_vec()]).unwrap();
    let message = message_from_fetch(&rows[0], &MailboxId::try_from("INBOX").unwrap(), 1);
    assert_eq!(
        message.envelope.subject.as_deref(),
        Some("新しい情報をお知らせします")
    );
    assert_eq!(
        message
            .envelope
            .from
            .first()
            .and_then(|a| a.name.as_deref()),
        Some("カスタマーサポート")
    );
}

#[test]
fn mailbox_from_list_maps_inbox_special_use_and_roleless() {
    let rows = crate::parse::parse_list(&[
        br#"LIST (\HasNoChildren) "/" "INBOX""#.to_vec(),
        br#"LIST (\HasNoChildren \Sent) "/" "Sent""#.to_vec(),
        br#"LIST (\HasNoChildren) "/" "Archive""#.to_vec(),
    ])
    .unwrap();
    let mailboxes: Vec<_> = rows
        .iter()
        .filter_map(|row| mailbox_from_list(row, true))
        .collect();

    let inbox = mailboxes.iter().find(|m| m.name == "INBOX").unwrap();
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    let sent = mailboxes.iter().find(|m| m.name == "Sent").unwrap();
    assert_eq!(sent.role, Some(MailboxRole::Sent));
    // The custom Archive folder has no SPECIAL-USE attribute → roleless.
    let archive = mailboxes.iter().find(|m| m.name == "Archive").unwrap();
    assert_eq!(archive.role, None);
}

#[test]
fn the_inbox_is_listed_as_inbox_however_the_server_spells_it() {
    // RFC 9051 §5.1: `INBOX` is case-insensitive, so `Inbox` is the same mailbox. A host binds
    // the inbox by its reserved name; an id in the server's spelling would sync it a second
    // time under another scope.
    let rows =
        crate::parse::parse_list(&[br#"LIST (\HasNoChildren) "/" "Inbox""#.to_vec()]).unwrap();
    let inbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(inbox.id.as_str(), "INBOX");
    assert_eq!(inbox.role, Some(MailboxRole::Inbox));
    assert_eq!(inbox.name, "Inbox");
}

#[test]
fn a_folder_inside_the_inbox_names_inbox_as_its_parent() {
    // Courier nests every folder under the inbox. The child's own id is the server's path, which
    // is what `SELECT` takes; only the reserved top-level name is folded.
    let rows = crate::parse::parse_list(&[
        br#"LIST (\HasChildren) "." "Inbox""#.to_vec(),
        br#"LIST (\HasNoChildren) "." "Inbox.Receipts""#.to_vec(),
    ])
    .unwrap();
    let child = mailbox_from_list(&rows[1], true).unwrap();
    assert_eq!(child.id.as_str(), "Inbox.Receipts");
    assert_eq!(child.parent.as_ref().unwrap().as_str(), "INBOX");
}

#[test]
fn a_noselect_container_is_listed_but_not_selectable() {
    // Gmail's `[Gmail]` and any server's parent of a nested folder it holds no mail in: the row
    // is a level of the tree, so it stays in the list to keep its children under it, and
    // `SELECT` on it is an error.
    let rows = crate::parse::parse_list(&[
        br#"LIST (\Noselect \HasChildren) "/" "[Gmail]""#.to_vec(),
        br#"LIST (\NoSelect \HasChildren) "/" "Work""#.to_vec(),
        br#"LIST (\HasNoChildren \All) "/" "[Gmail]/All Mail""#.to_vec(),
    ])
    .unwrap();
    let mailboxes: Vec<_> = rows
        .iter()
        .filter_map(|row| mailbox_from_list(row, true))
        .collect();

    let selectable: Vec<_> = mailboxes
        .iter()
        .map(|m| (m.id.as_str(), m.selectable))
        .collect();
    assert_eq!(
        selectable,
        [
            ("[Gmail]", false),
            ("Work", false),
            ("[Gmail]/All Mail", true)
        ]
    );
}

#[test]
fn a_nonexistent_level_is_a_container_while_a_listed_folder_sits_beneath_it() {
    // Dovecot's answer to the extended `LIST` for a folder made inside a parent nobody made:
    // the parent is `\NonExistent`, where the plain `LIST` calls it `\Noselect`.
    let rows = crate::parse::parse_list(&[
        br#"LIST (\NonExistent) "/" Parent"#.to_vec(),
        br#"LIST () "/" Parent/Child"#.to_vec(),
        br#"LIST (\NonExistent) "/" A"#.to_vec(),
        br#"LIST (\NonExistent) "/" A/B"#.to_vec(),
        br#"LIST () "/" A/B/C"#.to_vec(),
        // Nothing listed beneath either, so neither is a level of anything.
        br#"LIST (\NonExistent) "/" Gone"#.to_vec(),
        br#"LIST (\NonExistent) "/" Gone/Too"#.to_vec(),
    ])
    .unwrap();

    let kept = mailboxes_from_list(&rows, true);
    let kept: Vec<_> = kept
        .iter()
        .map(|(_, m)| (m.id.as_str(), m.selectable))
        .collect();
    assert_eq!(
        kept,
        [
            ("Parent", false),
            ("Parent/Child", true),
            ("A", false),
            ("A/B", false),
            ("A/B/C", true),
        ]
    );
}

#[test]
fn hierarchy_parent_is_derived_from_the_delimiter() {
    let rows = crate::parse::parse_list(&[br#"LIST () "/" "Work/Clients""#.to_vec()]).unwrap();
    let mailbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(mailbox.parent.as_ref().unwrap().as_str(), "Work");
    // The id is the whole path, because that is what `SELECT` takes; the display name is the
    // folder's own, because a pane indents it under the parent the id names.
    assert_eq!(mailbox.id.as_str(), "Work/Clients");
    assert_eq!(mailbox.name, "Clients");
}

#[test]
fn a_rooted_namespace_keeps_its_top_level_folders_at_the_top() {
    // Cyrus roots at the delimiter and Courier under `INBOX`. A leading delimiter is part of
    // the name rather than a separator, so neither shape invents a nameless parent.
    for raw in [
        br#"LIST () "/" "/Archive""#.to_vec(),
        br#"LIST () "." ".Archive""#.to_vec(),
    ] {
        let rows = crate::parse::parse_list(&[raw]).unwrap();
        let mailbox = mailbox_from_list(&rows[0], true).unwrap();
        assert_eq!(mailbox.parent, None);
        assert_eq!(mailbox.name, mailbox.id.as_str());
    }
}

#[test]
fn an_escaped_delimiter_is_split_on_like_any_other() {
    // Proton Mail Bridge escapes a delimiter inside a label name (`29\/01\/2021`) and then
    // lists a row per fragment as well, so the fragments are mailboxes whether or not the
    // escape is honoured. Splitting puts the chain on screen the way Thunderbird and Apple
    // Mail draw it; honouring the escape would leave Bridge's own fragment rows beside it as
    // siblings, which is three folders for one label. The bytes reach here at all because
    // `tokenize` keeps an unrecognised escape rather than refusing the response.
    let rows =
        crate::parse::parse_list(&[br#"LIST () "/" "Labels/example.com 29\/01\/2021""#.to_vec()])
            .unwrap();
    let mailbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(
        mailbox.parent.as_ref().unwrap().as_str(),
        r"Labels/example.com 29\/01\"
    );
    assert_eq!(mailbox.id.as_str(), r"Labels/example.com 29\/01\/2021");
    assert_eq!(mailbox.name, "2021");
}

#[test]
fn a_flat_namespace_keeps_the_whole_name() {
    // `LIST () NIL "Work/Clients"`: no delimiter, so the slash is a character in the name and
    // splitting on it would strip half of what the user called the folder.
    let rows = crate::parse::parse_list(&[br#"LIST () NIL "Work/Clients""#.to_vec()]).unwrap();
    let mailbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(mailbox.parent, None);
    assert_eq!(mailbox.name, "Work/Clients");
}

#[test]
fn a_folder_accepts_children_only_with_a_delimiter_and_without_noinferiors() {
    let accepts = |line: &[u8]| {
        let rows = crate::parse::parse_list(&[line.to_vec()]).unwrap();
        mailbox_from_list(&rows[0], true).unwrap().accepts_children
    };
    assert!(accepts(br#"LIST (\HasNoChildren) "/" "Work""#));
    // RFC 3501 §7.2.2: no child level exists or can be created.
    assert!(!accepts(br#"LIST (\Noinferiors \Trash) "/" "Trash""#));
    // A flat namespace has nowhere to put a level.
    assert!(!accepts(br#"LIST () NIL "Work""#));
}

#[test]
fn a_nested_folder_named_like_a_role_is_not_that_role() {
    // Role matching reads the whole path: `INBOX` is reserved at the top level only, so a
    // folder someone called `INBOX` inside another one is an ordinary folder.
    let rows = crate::parse::parse_list(&[br#"LIST () "/" "Work/INBOX""#.to_vec()]).unwrap();
    let mailbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(mailbox.role, None);
    assert_eq!(mailbox.name, "INBOX");
}

#[test]
fn a_deeply_nested_name_keeps_only_its_last_segment() {
    let rows =
        crate::parse::parse_list(&[br#"LIST () "/" "Work/Clients/Acme/2024""#.to_vec()]).unwrap();
    let mailbox = mailbox_from_list(&rows[0], true).unwrap();
    assert_eq!(mailbox.name, "2024");
    assert_eq!(
        mailbox.parent.as_ref().unwrap().as_str(),
        "Work/Clients/Acme"
    );
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

#[test]
fn parse_message_key_round_trips_with_message_key() {
    // The inverse of `message_key`: a synthesized key parses back to its triple.
    assert_eq!(
        parse_message_key(message_key("Sent", 7, 42).as_str()),
        Some(("Sent", 7, 42))
    );
    // A mailbox name carrying the delimiters is still recovered whole: `:u` splits
    // on its first occurrence and `@` on the first, so a mailbox like "a@b" or one
    // spelled like a UID suffix survives.
    assert_eq!(
        parse_message_key("imap:v1:u5@Work/a@b"),
        Some(("Work/a@b", 1, 5))
    );
    // Garbage and foreign keys yield None, never a panic.
    assert!(parse_message_key("not-an-imap-key").is_none());
    assert!(parse_message_key("imap:v1:u5").is_none()); // no @mailbox
    assert!(parse_message_key("imap:vX:u5@INBOX").is_none()); // non-decimal validity
    assert!(parse_message_key("imap:v1:uY@INBOX").is_none()); // non-decimal uid
    assert!(parse_message_key("imap:v1:u5@").is_none()); // empty mailbox
    assert!(parse_message_key("").is_none());
}

#[test]
fn keyword_to_flag_inverts_flags_to_keywords() {
    // The four standard system keywords map back to their backslash system flags.
    assert_eq!(
        keyword_to_flag(&Keyword::system(SystemKeyword::Seen)),
        "\\Seen"
    );
    assert_eq!(
        keyword_to_flag(&Keyword::system(SystemKeyword::Flagged)),
        "\\Flagged"
    );
    assert_eq!(
        keyword_to_flag(&Keyword::system(SystemKeyword::Answered)),
        "\\Answered"
    );
    assert_eq!(
        keyword_to_flag(&Keyword::system(SystemKeyword::Draft)),
        "\\Draft"
    );
    // Another system keyword (`$forwarded`) and a custom keyword both pass through
    // as bare IMAP keyword atoms (their `as_str`).
    assert_eq!(
        keyword_to_flag(&Keyword::system(SystemKeyword::Forwarded)),
        "$forwarded"
    );
    assert_eq!(
        keyword_to_flag(&Keyword::new("harness").unwrap()),
        "harness"
    );
}
