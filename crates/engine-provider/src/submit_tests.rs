//! Unit tests for the outbound submission shapes: the builders, and the durable-payload
//! round trip every field must survive (a `Draft` is stored as an outbox `PendingOp`
//! before the send). A sibling file so `submit.rs` stays under the line limit.

use super::*;

fn mid(value: &str) -> MessageIdHeader {
    MessageIdHeader::new(value).unwrap()
}

fn draft() -> Draft {
    Draft::new(
        mid("reply@host"),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Re: hi",
        "thanks",
    )
}

#[test]
fn new_defaults_the_threading_linkage_to_none() {
    let draft = draft();
    assert_eq!(draft.in_reply_to, None);
    assert!(draft.references.is_empty());
}

#[test]
fn in_reply_to_builder_sets_parent_and_references() {
    let draft = draft().in_reply_to(
        mid("parent@host"),
        vec![mid("root@host"), mid("parent@host")],
    );
    assert_eq!(draft.in_reply_to, Some(mid("parent@host")));
    assert_eq!(draft.references, vec![mid("root@host"), mid("parent@host")]);
}

#[test]
fn new_defaults_cc_and_bcc_to_empty() {
    let draft = draft();
    assert!(draft.cc.is_empty());
    assert!(draft.bcc.is_empty());
}

#[test]
fn cc_and_bcc_builders_set_recipients() {
    let draft = draft()
        .with_cc(vec![EmailAddress::new("carol@test.local")])
        .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    assert_eq!(draft.cc, vec![EmailAddress::new("carol@test.local")]);
    assert_eq!(draft.bcc, vec![EmailAddress::new("dave@test.local")]);
}

#[test]
fn cc_and_bcc_round_trip_through_serde() {
    let draft = draft()
        .with_cc(vec![EmailAddress::new("carol@test.local")])
        .with_bcc(vec![EmailAddress::new("dave@test.local")]);
    let json = serde_json::to_string(&draft).unwrap();
    let restored: Draft = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.cc, draft.cc);
    assert_eq!(restored.bcc, draft.bcc);
}

#[test]
fn a_payload_without_cc_or_bcc_still_deserializes() {
    // A durable outbox payload serialized before Cc/Bcc support omits the new fields;
    // `#[serde(default)]` keeps it loadable with empty recipient lists.
    let json = r#"{
        "message_id": "old@host",
        "from": {"email": "alice@test.local"},
        "to": [{"email": "bob@test.local"}],
        "subject": "hi",
        "text_body": "body"
    }"#;
    let restored: Draft = serde_json::from_str(json).unwrap();
    assert!(restored.cc.is_empty());
    assert!(restored.bcc.is_empty());
}

#[test]
fn serde_round_trip_preserves_the_threading_fields() {
    let draft = draft().in_reply_to(
        mid("parent@host"),
        vec![mid("root@host"), mid("parent@host")],
    );
    let json = serde_json::to_string(&draft).unwrap();
    let restored: Draft = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, draft);
    assert_eq!(restored.in_reply_to, Some(mid("parent@host")));
    assert_eq!(
        restored.references,
        vec![mid("root@host"), mid("parent@host")]
    );
}

#[test]
fn a_payload_without_the_threading_fields_still_deserializes() {
    // A durable outbox payload serialized before threading support omits the new
    // fields; `#[serde(default)]` keeps it loadable as a non-reply draft.
    let json = r#"{
        "message_id": "old@host",
        "from": {"email": "alice@test.local"},
        "to": [{"email": "bob@test.local"}],
        "subject": "hi",
        "text_body": "body"
    }"#;
    let restored: Draft = serde_json::from_str(json).unwrap();
    assert_eq!(restored.in_reply_to, None);
    assert!(restored.references.is_empty());
}

#[test]
fn a_payload_without_rich_body_fields_still_deserializes() {
    // A durable outbox payload serialized before rich-body support omits the new
    // fields; defaults keep it loadable as a plain-text draft.
    let json = r#"{
        "message_id": "old@host",
        "from": {"email": "alice@test.local"},
        "to": [{"email": "bob@test.local"}],
        "subject": "hi",
        "text_body": "body"
    }"#;
    let restored: Draft = serde_json::from_str(json).unwrap();
    assert_eq!(restored.html_body, None);
    assert!(restored.attachments.is_empty());
    assert_eq!(restored.calendar, None);
}

#[test]
fn an_itip_object_is_a_body_part_not_an_attachment() {
    // The distinction the whole type exists for. An attachment part carries a
    // `Content-Disposition` and a file name and no `method=` — an answer sent that way
    // is filed as `invite.ics` and never dispatched as a reply.
    let draft = draft().with_calendar(DraftCalendar::new(
        ScheduleMethod::Reply,
        "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n",
    ));
    let calendar = draft.calendar.as_ref().unwrap();
    assert_eq!(calendar.method, ScheduleMethod::Reply);
    assert!(
        draft.attachments.is_empty(),
        "an iTIP object must not travel through the attachment list"
    );
}

#[test]
fn an_itip_draft_survives_the_durable_payload_round_trip() {
    // The send is outbox-mediated: a durable `PendingOp` precedes it, so a restart
    // between enqueue and send must read back the same object *and* the same method —
    // a message whose `method=` was lost is no longer an iMIP message.
    let draft = draft().with_calendar(DraftCalendar::new(
        ScheduleMethod::Reply,
        "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n",
    ));
    let json = serde_json::to_string(&draft).unwrap();
    assert_eq!(serde_json::from_str::<Draft>(&json).unwrap(), draft);
}

#[test]
fn rich_draft_builders_set_html_and_attachments() {
    let inline = DraftAttachment::inline(
        "chart.png",
        "image/png",
        ContentIdHeader::new("chart.1@test.local").unwrap(),
        vec![1, 2, 3],
    );
    let file = DraftAttachment::attachment("report.pdf", "application/pdf", vec![4, 5]);

    let draft = draft()
        .with_html_body("<p>thanks</p>")
        .with_attachment(inline.clone())
        .with_attachment(file.clone());

    assert_eq!(draft.html_body.as_deref(), Some("<p>thanks</p>"));
    assert_eq!(draft.attachments, vec![inline, file]);
}
