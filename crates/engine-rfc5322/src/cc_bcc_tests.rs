//! Tests for the `Cc`/`Bcc` headers in the assembled message.
//!
//! A `Cc` header is always emitted when present (visible to everyone), but `Bcc` is
//! emitted ONLY in the filed Sent/Drafts copy ([`assemble_filed_message`]), never in
//! the over-the-wire message ([`assemble_message`]) — so recipients can't see the Bcc
//! list while the sender's Sent folder still records it (Outlook/Thunderbird behavior).

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::Draft;
use time::macros::datetime;

use super::*;

fn draft_with_cc_bcc() -> Draft {
    Draft::new(
        MessageIdHeader::new("send@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject line",
        "body",
    )
    .with_cc(vec![EmailAddress::new("carol@test.local")])
    .with_bcc(vec![EmailAddress::new("dave@test.local")])
}

fn wire(draft: &Draft) -> String {
    String::from_utf8(assemble_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap()).unwrap()
}

fn stored_draft(draft: &Draft) -> String {
    String::from_utf8(assemble_draft_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap())
        .unwrap()
}

fn filed(draft: &Draft) -> String {
    String::from_utf8(assemble_filed_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap())
        .unwrap()
}

#[test]
fn wire_message_emits_cc_but_never_bcc() {
    let text = wire(&draft_with_cc_bcc());
    assert!(text.contains("To: bob@test.local\r\n"), "{text}");
    assert!(text.contains("Cc: carol@test.local\r\n"), "{text}");
    // A recipient can't see the Bcc list: no Bcc header, and the Bcc address appears NOWHERE
    // in the transmitted message.
    assert!(!text.contains("Bcc:"), "{text}");
    assert!(!text.contains("dave@test.local"), "{text}");
}

#[test]
fn filed_copy_includes_the_bcc_header_for_the_sender() {
    let text = filed(&draft_with_cc_bcc());
    // The sender's Sent/Drafts copy keeps To, Cc, AND Bcc, so they can see whom they Bcc'd.
    assert!(text.contains("To: bob@test.local\r\n"), "{text}");
    assert!(text.contains("Cc: carol@test.local\r\n"), "{text}");
    assert!(text.contains("Bcc: dave@test.local\r\n"), "{text}");
}

#[test]
fn a_bcc_only_message_uses_undisclosed_recipients_for_the_empty_to() {
    // No To, no Cc — only Bcc. The message still needs a valid To header: name an empty group
    // (Outlook/Thunderbird behavior) rather than emit a bare empty `To:`. The wire copy still
    // hides the Bcc.
    let draft = Draft::new(
        MessageIdHeader::new("send@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        Vec::new(),
        "Subject line",
        "body",
    )
    .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    let text = wire(&draft);
    assert!(text.contains("To: undisclosed-recipients:;\r\n"), "{text}");
    assert!(!text.contains("Bcc:"), "{text}");
    assert!(!text.contains("dave@test.local"), "{text}");
}

#[test]
fn the_two_assemblies_are_identical_when_there_is_no_bcc() {
    // With no Bcc the wire and filed copies are byte-identical (the Bcc header is the only
    // difference), which is why a caller can reuse the wire bytes for the Sent copy then.
    let draft = draft_with_cc_bcc().with_bcc(Vec::new());
    assert_eq!(wire(&draft), filed(&draft));
}

#[test]
fn a_draft_with_no_to_recipients_carries_no_to_header() {
    // A draft is often saved before anyone is addressed. It names the recipients it has, so it
    // carries no `To` at all rather than the empty group a Bcc-only *send* needs: Gmail's
    // `drafts.create` refuses that group with HTTP 400 "Invalid To header" (observed), and a
    // client resuming the draft would otherwise read the group's name as a recipient.
    let draft = Draft::new(
        MessageIdHeader::new("draft@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        Vec::new(),
        "Subject line",
        "body",
    )
    .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    let text = stored_draft(&draft);
    assert!(!text.contains("To:"), "{text}");
    assert!(text.contains("Bcc: dave@test.local\r\n"), "{text}");

    // The Sent copy of the same message as a send keeps the group: it records what went out.
    let sent = filed(&draft);
    assert!(sent.contains("To: undisclosed-recipients:;\r\n"), "{sent}");
}

#[test]
fn a_draft_with_to_recipients_is_the_filed_copy() {
    let draft = Draft::new(
        MessageIdHeader::new("draft@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject line",
        "body",
    )
    .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    assert_eq!(stored_draft(&draft), filed(&draft));
}
