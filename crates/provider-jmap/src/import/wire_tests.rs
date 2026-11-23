//! The request an import sends, and what each answer means per message: Stalwart's captured
//! answers first, then the RFC's shapes the harness cannot be made to produce.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    ids::{BlobId, MailboxId},
    mail::{Keyword, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    time::UtcDateTime,
};
use engine_provider::{ProviderResult, TargetRefusal, import::MessageImport};
use serde_json::{Value, json};

use super::*;
use crate::provider::provider_test_support::fixture;

fn message(bytes: &str, mailboxes: &[&str]) -> MessageImport {
    MessageImport::new(
        RawMime::new(bytes.as_bytes().to_vec()),
        Memberships::new(mailboxes.iter().map(|id| MailboxId::try_from(*id).unwrap())).unwrap(),
    )
}

fn dated(message: MessageImport) -> MessageImport {
    let keywords: BTreeSet<Keyword> = ["$seen", "$label1"]
        .into_iter()
        .map(|k| Keyword::new(k).unwrap())
        .collect();
    message.with_metadata(
        MessageMetadata::default()
            .with_keywords(keywords)
            .with_received_at("2019-02-03T04:05:06Z".parse::<UtcDateTime>().unwrap()),
    )
}

fn inline(index: usize, message: &MessageImport) -> Entry<'_> {
    Entry {
        index,
        message,
        blob: Blob::Inline,
    }
}

fn account() -> JmapAccountId {
    JmapAccountId::new("e")
}

fn read_fixture(name: &str, entries: &[Entry<'_>]) -> Vec<ProviderResult<ImportedMessage>> {
    let built = build(&account(), entries);
    let response = Response::parse(&fixture(name)).unwrap();
    read(&built, &response, entries).answers
}

fn read_whole(answers: &Value, entries: &[Entry<'_>]) -> Read {
    let built = build(&account(), entries);
    let response = Response::parse(&json!({ "methodResponses": answers })).unwrap();
    read(&built, &response, entries)
}

fn read_answer(answers: &Value, entries: &[Entry<'_>]) -> Vec<ProviderResult<ImportedMessage>> {
    read_whole(answers, entries).answers
}

#[test]
fn an_inline_message_is_uploaded_in_the_request_and_referenced_by_its_creation_id() {
    let one = dated(message("From: a@example.com\r\n\r\nbody", &["ec"]));
    let built = build(&account(), &[inline(3, &one)]);
    let wire = built.request.to_json();

    assert_eq!(
        wire["using"],
        json!([capability::CORE, capability::MAIL, capability::BLOB])
    );
    let upload = &wire["methodCalls"][0];
    assert_eq!(upload[0], "Blob/upload");
    assert_eq!(upload[1]["accountId"], "e");
    let data = upload[1]["create"]["b3"]["data"][0]["data:asBase64"]
        .as_str()
        .unwrap();
    assert_eq!(data, engine_rfc5322::base64_encode(one.raw.as_bytes()));
    assert_eq!(upload[1]["create"]["b3"]["type"], "message/rfc822");

    let import = &wire["methodCalls"][1];
    assert_eq!(import[0], "Email/import");
    assert_eq!(
        import[1]["emails"]["m3"],
        json!({
            "blobId": "#b3",
            "mailboxIds": { "ec": true },
            "keywords": { "$label1": true, "$seen": true },
            "receivedAt": "2019-02-03T04:05:06Z",
        })
    );
}

#[test]
fn an_uploaded_message_is_referenced_by_its_blob_id_and_an_undated_one_carries_no_time() {
    let one = message("x", &["a", "b"]);
    let entries = [Entry {
        index: 0,
        message: &one,
        blob: Blob::Uploaded(BlobId::try_from("G1").unwrap()),
    }];
    let wire = build(&account(), &entries).request.to_json();

    // Nothing inline: no `Blob/upload`, and no blob capability declared.
    assert_eq!(wire["using"], json!([capability::CORE, capability::MAIL]));
    assert_eq!(wire["methodCalls"].as_array().unwrap().len(), 1);
    assert_eq!(
        wire["methodCalls"][0][1]["emails"]["m0"],
        json!({ "blobId": "G1", "mailboxIds": { "a": true, "b": true }, "keywords": {} })
    );
}

/// An entry is sized without its blob id, which the plan adds by route.
#[test]
fn an_entry_is_sized_but_for_its_blob_id() {
    let one = dated(message("x", &["ec"]));
    let longest = String::new();
    // As it sits in the `emails` map: its key, a colon, the entry, and a comma.
    let entry = serde_json::to_string(&email_entry(&one, &longest)).unwrap();
    let wire = format!("\"m12\":{entry},").len() as u64;
    assert!(
        entry_bytes(12, &one) >= wire,
        "{} < {wire}",
        entry_bytes(12, &one)
    );
}

#[test]
fn stalwart_stores_one_and_refuses_one_naming_the_missing_mailbox() {
    let (one, two) = (dated(message("1", &["ec"])), message("2", &["zzzzzzz"]));
    let answers = read_fixture(
        "email_import_response.json",
        &[inline(0, &one), inline(1, &two)],
    );

    let stored = answers[0].as_ref().unwrap();
    assert_eq!(stored.key.as_ref().unwrap().as_str(), "2qaaaahe");
    assert!(!stored.already_there);
    assert_eq!(stored.kept, one.metadata, "what was asked is what was kept");

    // A mailbox that does not exist is the target's state, as on IMAP (`providers.md`, "Import").
    let refused = answers[1].as_ref().unwrap_err();
    assert_eq!(
        refused.target_refusal(),
        Some(TargetRefusal::NoSuchContainer)
    );
    assert!(!refused.is_item_unavailable(), "a mailbox, not the message");
    // Stalwart's description names the mailbox ("Mailbox bzzzzzz does not exist.").
    assert!(!refused.detail().contains("bzzzzzz"), "no server text");
}

/// The upload fails as a whole method, and the import with it: every message of the call is
/// refused for the request's sake, which a resend of the same request would meet again.
#[test]
fn stalwart_refusing_the_whole_upload_answers_every_message_with_that_refusal() {
    let (one, two) = (message("1", &["ec"]), message("2", &["ec"]));
    let answers = read_fixture(
        "email_import_blob_refused_response.json",
        &[inline(0, &one), inline(1, &two)],
    );
    for answer in &answers {
        let err = answer.as_ref().unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(err.detail().contains("invalidArguments"), "{err}");
    }
}

fn uploaded(ids: &[&str], refused: &Value) -> Value {
    let created: serde_json::Map<String, Value> = ids
        .iter()
        .map(|id| ((*id).to_owned(), json!({ "id": format!("blob-{id}") })))
        .collect();
    json!(["Blob/upload", { "accountId": "e", "created": created, "notCreated": refused }, "0"])
}

/// RFC 9404 refuses a blob on its own (`notCreated`); the cause is that refusal, and a message
/// whose blob was fine was not stored either when the import then fails whole.
#[test]
fn a_refused_blob_is_its_message_s_answer_and_the_rest_of_the_call_is_retryable() {
    let (a, b) = (message("1", &["x"]), message("2", &["x"]));
    let read = read_whole(
        &json!([
            uploaded(&["b1"], &json!({ "b0": { "type": "tooLarge" } })),
            ["error", { "type": "invalidResultReference" }, "1"]
        ]),
        &[inline(0, &a), inline(1, &b)],
    );
    assert!(read.answers[0].as_ref().unwrap_err().is_item_unavailable());
    let bystander = read.answers[1].as_ref().unwrap_err();
    assert_eq!(bystander.class(), FailureClass::Retryable);
    assert!(!bystander.requires_confirmation());
    assert!(read.ending.is_none(), "one message's refusal ends nothing");
}

/// The upload quota refusing an inline blob is the wall a `429` from the upload URL is: the
/// batch ends, and the message beside it carries the throttle as not sent.
#[test]
fn an_inline_blob_over_the_upload_quota_ends_the_batch() {
    let (a, b) = (message("1", &["x"]), message("2", &["x"]));
    let read = read_whole(
        &json!([
            uploaded(&["b1"], &json!({ "b0": { "type": "overQuota" } })),
            ["error", { "type": "invalidResultReference" }, "1"]
        ]),
        &[inline(0, &a), inline(1, &b)],
    );
    assert_eq!(
        read.answers[0].as_ref().unwrap_err().class(),
        FailureClass::RateLimited
    );
    let beside = read.answers[1].as_ref().unwrap_err();
    assert_eq!(beside.class(), FailureClass::RateLimited);
    assert!(beside.detail().starts_with("not sent"), "{beside}");
    assert_eq!(
        read.ending.as_ref().map(ProviderError::class),
        Some(FailureClass::RateLimited)
    );
}

/// Only `invalidResultReference` says the import failed for its references and stored
/// nothing; any other failure of the call says what it says, and two say "may be stored".
#[test]
fn beside_a_refused_blob_only_a_reference_failure_means_not_stored() {
    let (a, b) = (message("1", &["x"]), message("2", &["x"]));
    let entries = [inline(0, &a), inline(1, &b)];
    let refused = || uploaded(&["b1"], &json!({ "b0": { "type": "tooLarge" } }));
    for answers in [
        json!([refused()]),
        json!([refused(), ["error", { "type": "serverPartialFail" }, "1"]]),
    ] {
        let read = read_whole(&answers, &entries);
        let beside = read.answers[1].as_ref().unwrap_err();
        assert!(beside.requires_confirmation(), "{answers}: {beside}");
    }
}

#[test]
fn a_refused_blob_is_the_answer_where_the_import_refuses_its_entry_too() {
    let (a, b) = (message("1", &["x"]), message("2", &["x"]));
    let answers = read_answer(
        &json!([
            uploaded(&["b1"], &json!({ "b0": { "type": "invalidProperties" } })),
            ["Email/import", {
                "created": { "m1": { "id": "M1" } },
                "notCreated": { "m0": { "type": "invalidProperties" } }
            }, "1"]
        ]),
        &[inline(0, &a), inline(1, &b)],
    );
    assert!(answers[0].as_ref().unwrap_err().detail().contains("b0"));
    assert_eq!(
        answers[1].as_ref().unwrap().key.as_ref().unwrap().as_str(),
        "M1"
    );
}

/// RFC 8621 §4.8: a server that forbids duplicates answers `alreadyExists` with the id it holds.
#[test]
fn already_there_is_a_link_not_a_failure() {
    let (a, b) = (message("1", &["x"]), message("2", &["x"]));
    let answers = read_answer(
        &json!([
            uploaded(&["b0", "b1"], &json!({})),
            ["Email/import", { "notCreated": {
                "m0": { "type": "alreadyExists", "existingId": "E9" },
                "m1": { "type": "alreadyExists" }
            } }, "1"]
        ]),
        &[inline(0, &a), inline(1, &b)],
    );
    let there = answers[0].as_ref().unwrap();
    assert!(there.already_there);
    assert_eq!(there.key.as_ref().unwrap().as_str(), "E9");
    assert_eq!(there.kept, MessageMetadata::default());
    // The RFC requires the id. Without it the server still says the message is there, so the
    // answer is to find it, not to send it again.
    assert!(answers[1].as_ref().unwrap_err().requires_confirmation());
}

#[test]
fn a_message_the_target_will_never_take_is_unavailable_and_an_expired_blob_retryable() {
    let messages: Vec<_> = (0..5).map(|n| message(&n.to_string(), &["x"])).collect();
    let entries: Vec<_> = messages
        .iter()
        .enumerate()
        .map(|(n, m)| inline(n, m))
        .collect();
    let answers = read_answer(
        &json!([
            uploaded(&["b0", "b1", "b2", "b3", "b4"], &json!({})),
            ["Email/import", { "notCreated": {
                "m0": { "type": "invalidEmail" },
                "m1": { "type": "tooManyMailboxes" },
                "m2": { "type": "tooManyKeywords" },
                "m3": { "type": "blobNotFound" },
            } }, "1"]
        ]),
        &entries,
    );
    for unavailable in &answers[..3] {
        assert!(unavailable.as_ref().unwrap_err().is_item_unavailable());
    }
    assert_eq!(
        answers[3].as_ref().unwrap_err().class(),
        FailureClass::Retryable
    );
    // Named in neither map: it may be stored.
    assert!(answers[4].as_ref().unwrap_err().requires_confirmation());
}

#[test]
fn a_partial_failure_or_a_missing_answer_may_have_stored_the_message() {
    let a = message("1", &["x"]);
    let entries = [inline(0, &a)];
    let partial = read_answer(
        &json!([uploaded(&["b0"], &json!({})), ["error", { "type": "serverPartialFail" }, "1"]]),
        &entries,
    );
    assert!(partial[0].as_ref().unwrap_err().requires_confirmation());

    let missing = read_answer(&json!([uploaded(&["b0"], &json!({}))]), &entries);
    assert!(missing[0].as_ref().unwrap_err().requires_confirmation());

    let unstored = read_answer(
        &json!([uploaded(&["b0"], &json!({})), ["error", { "type": "rateLimit" }, "1"]]),
        &entries,
    );
    let throttled = unstored[0].as_ref().unwrap_err();
    assert_eq!(throttled.class(), FailureClass::RateLimited);
    assert!(!throttled.requires_confirmation());
}

#[test]
fn an_upload_that_answers_nothing_for_a_blob_did_not_store_its_message() {
    let a = message("1", &["x"]);
    let answers = read_answer(
        &json!([
            uploaded(&[], &json!({})),
            ["error", { "type": "invalidResultReference" }, "1"]
        ]),
        &[inline(0, &a)],
    );
    let err = answers[0].as_ref().unwrap_err();
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[test]
fn an_unusable_created_id_may_have_stored_the_message() {
    let a = message("1", &["x"]);
    let answers = read_answer(
        &json!([
            uploaded(&["b0"], &json!({})),
            ["Email/import", { "created": { "m0": { "id": "" } } }, "1"]
        ]),
        &[inline(0, &a)],
    );
    assert!(answers[0].as_ref().unwrap_err().requires_confirmation());
}

/// The target's own state refusing a message is named as on every adapter (`providers.md`,
/// "Import"): a full account, a mailbox that does not exist, one the credential may not write.
/// Any other property refused is the request's.
#[test]
fn a_refusal_by_the_target_s_state_is_named() {
    let messages: Vec<_> = (0..4).map(|n| message(&n.to_string(), &["x"])).collect();
    let entries: Vec<_> = messages
        .iter()
        .enumerate()
        .map(|(n, m)| inline(n, m))
        .collect();
    let answers = read_answer(
        &json!([
            uploaded(&["b0", "b1", "b2", "b3"], &json!({})),
            ["Email/import", { "notCreated": {
                "m0": { "type": "overQuota" },
                "m1": { "type": "invalidProperties", "properties": ["mailboxIds"] },
                "m2": { "type": "forbidden" },
                "m3": { "type": "invalidProperties", "properties": ["keywords"] },
            } }, "1"]
        ]),
        &entries,
    );
    for (answer, reason) in answers.iter().zip([
        TargetRefusal::StorageFull,
        TargetRefusal::NoSuchContainer,
        TargetRefusal::ReadOnlyContainer,
    ]) {
        let refused = answer.as_ref().unwrap_err();
        assert_eq!(refused.target_refusal(), Some(reason), "{refused}");
        assert_eq!(refused.class(), FailureClass::InvalidState);
        assert_eq!(refused.retry_after(), None);
    }
    let request = answers[3].as_ref().unwrap_err();
    assert_eq!(request.class(), FailureClass::Permanent);
    assert_eq!(request.target_refusal(), None);

    let whole = read_answer(
        &json!([uploaded(&["b0"], &json!({})), ["error", { "type": "overQuota" }, "1"]]),
        &entries[..1],
    );
    assert_eq!(
        whole[0].as_ref().unwrap_err().target_refusal(),
        Some(TargetRefusal::StorageFull)
    );
}
