//! Offline tests for the JMAP send request: the draft create, the submission, and the
//! receipt parse.

use engine_core::error::FailureClass;

use super::*;

fn send_response() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/submit_send_response.json")).unwrap()
}

fn results(doc: &Value) -> (Value, Value) {
    // methodResponses: [Email/set "0", EmailSubmission/set "1", implicit Email/set "1"]
    let responses = doc["methodResponses"].as_array().unwrap();
    (responses[0][1].clone(), responses[1][1].clone())
}

fn message_id() -> MessageIdHeader {
    MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap()
}

#[test]
fn parses_the_sent_email_key_and_echoes_message_id() {
    let doc = send_response();
    let (email, submission) = results(&doc);
    let receipt = parse_receipt(&email, &submission, &message_id()).unwrap();
    // The created email id (kept across the Drafts→Sent move) is the resolved key.
    assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
    assert_eq!(receipt.message_id, message_id());
}

#[test]
fn email_set_error_classifies_and_aborts() {
    let email = json!({
        "notCreated": { "draft": { "type": "invalidProperties", "properties": ["from"] } }
    });
    let submission = json!({ "created": { "sub": { "id": "x" } } });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[test]
fn submission_error_classifies_after_email_created() {
    // The observed Stalwart failure when identityId is missing.
    let email = json!({ "created": { "draft": { "id": "e1" } } });
    let submission = json!({
        "notCreated": { "sub": { "type": "invalidProperties", "properties": ["identityId"] } }
    });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[test]
fn rate_limited_submission_is_retryable() {
    let email = json!({ "created": { "draft": { "id": "e1" } } });
    let submission = json!({ "notCreated": { "sub": { "type": "rateLimit" } } });
    let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
    assert!(err.failure_class().is_retryable());
}

#[test]
fn build_draft_targets_drafts_and_carries_message_id() {
    let context = SubmitContext {
        drafts: "d".to_owned(),
        sent: "e".to_owned(),
        identity: "b".to_owned(),
    };
    let draft = Draft::new(
        message_id(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    );
    let create = build_draft(&context.drafts, &draft, &[]);
    assert_eq!(create["mailboxIds"]["d"], json!(true));
    assert_eq!(create["keywords"]["$draft"], json!(true));
    assert_eq!(create["messageId"][0], "step4-send-probe-0002@test.local");
    assert_eq!(create["from"][0]["email"], "alice@test.local");

    let (submission, on_success) = build_submission(&context, &draft);
    assert_eq!(submission["emailId"], "#draft");
    assert_eq!(submission["identityId"], "b");
    // onSuccessUpdateEmail moves Drafts→Sent and clears $draft.
    assert_eq!(on_success["#sub"]["mailboxIds/d"], Value::Null);
    assert_eq!(on_success["#sub"]["mailboxIds/e"], json!(true));
    assert_eq!(on_success["#sub"]["keywords/$draft"], Value::Null);
}

fn mid(value: &str) -> MessageIdHeader {
    MessageIdHeader::new(value).unwrap()
}

fn addressed_draft() -> Draft {
    Draft::new(
        message_id(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Re: Subject",
        "Body",
    )
    .with_cc(vec![
        EmailAddress::named("Carol", "carol@test.local"),
        EmailAddress::new("BOB@test.local"),
    ])
    .with_bcc(vec![
        EmailAddress::new("dave@test.local"),
        EmailAddress::new("carol@TEST.local"),
    ])
    .in_reply_to(
        mid("parent@test.local"),
        vec![mid("root@test.local"), mid("parent@test.local")],
    )
}

/// The created Email is the sender's copy: it carries every recipient header, Bcc
/// included, as the filed copy on every other transport does. RFC 8621 §7.5 has the
/// server remove the Bcc header during delivery, so no recipient sees it.
#[test]
fn build_draft_carries_cc_bcc_and_the_threading_headers() {
    let create = build_draft("d", &addressed_draft(), &[]);
    assert_eq!(
        create["cc"],
        json!([
            { "name": "Carol", "email": "carol@test.local" },
            { "email": "BOB@test.local" },
        ])
    );
    assert_eq!(
        create["bcc"],
        json!([{ "email": "dave@test.local" }, { "email": "carol@TEST.local" }])
    );
    // `header:In-Reply-To:asMessageIds` form: msg-ids without angle brackets.
    assert_eq!(create["inReplyTo"], json!(["parent@test.local"]));
    assert_eq!(
        create["references"],
        json!(["root@test.local", "parent@test.local"])
    );
}

/// Absent headers are left out rather than written as empty lists, so an original
/// message is created exactly as before.
#[test]
fn build_draft_omits_the_headers_a_draft_does_not_have() {
    let draft = Draft::new(
        message_id(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    );
    let create = build_draft("d", &draft, &[]);
    for absent in ["cc", "bcc", "inReplyTo", "references"] {
        assert!(create.get(absent).is_none(), "{absent}: {create}");
    }
}

/// Every recipient is in the envelope, To + Cc + Bcc, deduplicated case-insensitively
/// and in that order, as the SMTP path builds `RCPT TO`.
#[test]
fn the_envelope_reaches_every_recipient_once() {
    let context = SubmitContext {
        drafts: "d".to_owned(),
        sent: "e".to_owned(),
        identity: "b".to_owned(),
    };
    let (submission, _) = build_submission(&context, &addressed_draft());
    assert_eq!(
        submission["envelope"]["rcptTo"],
        json!([
            { "email": "bob@test.local" },
            { "email": "carol@test.local" },
            { "email": "dave@test.local" },
        ])
    );
}

#[test]
fn build_draft_carries_html_as_alternative_body() {
    let context = SubmitContext {
        drafts: "d".to_owned(),
        sent: "e".to_owned(),
        identity: "b".to_owned(),
    };
    let draft = Draft::new(
        message_id(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Plain",
    )
    .with_html_body("<p>Plain</p>");

    let create = build_draft(&context.drafts, &draft, &[]);

    assert_eq!(create["bodyStructure"]["type"], "multipart/alternative");
    assert_eq!(create["bodyStructure"]["subParts"][0]["partId"], "text");
    assert_eq!(create["bodyStructure"]["subParts"][1]["partId"], "html");
    assert_eq!(create["bodyValues"]["text"]["value"], "Plain");
    assert_eq!(create["bodyValues"]["html"]["value"], "<p>Plain</p>");
}

fn keyword(value: &str) -> Keyword {
    Keyword::new(value).unwrap()
}

fn tagged_draft() -> Draft {
    addressed_draft()
        .with_sent_copy_keyword(keyword("project-x"))
        .with_sent_copy_keyword(keyword("a/b~c"))
}

#[test]
fn a_draft_without_sent_copy_keywords_asks_nothing() {
    assert!(keyword_update("c", "e1", &addressed_draft()).is_none());
}

#[test]
fn sent_copy_keywords_are_an_update_of_the_filed_copy() {
    let update = keyword_update("c", "e1", &tagged_draft()).unwrap();
    assert_eq!(update["accountId"], "c");
    assert_eq!(
        update["update"]["e1"],
        json!({ "keywords/project-x": true, "keywords/a~1b~0c": true })
    );
}

#[test]
fn the_created_email_carries_no_sent_copy_keyword() {
    // `build_draft` also saves drafts, which must not carry the sent copy's keywords; the
    // send sets them in a call of its own once the copy is filed.
    let create = build_draft("d", &tagged_draft(), &[]);
    assert_eq!(create["keywords"], json!({ "$draft": true, "$seen": true }));
}

#[test]
fn an_applied_keyword_update_keeps_every_keyword() {
    let result = json!({ "updated": { "bmaaaaal": null } });
    assert_eq!(
        kept_keywords(Ok(&result), &tagged_draft()),
        BTreeSet::from([keyword("project-x"), keyword("a/b~c")])
    );
}

#[test]
fn a_refused_keyword_update_keeps_none() {
    let refused = json!({ "notUpdated": { "e1": { "type": "forbidden" } } });
    assert!(kept_keywords(Ok(&refused), &tagged_draft()).is_empty());
    let silent = json!({});
    assert!(kept_keywords(Ok(&silent), &tagged_draft()).is_empty());
    let failed = JmapError::protocol("method error");
    assert!(kept_keywords(Err(failed), &tagged_draft()).is_empty());
}
