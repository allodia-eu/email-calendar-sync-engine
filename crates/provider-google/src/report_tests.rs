//! Offline tests for [`report_message`], routed to the captured real `modify`
//! responses.

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};
use engine_provider::{MessageReport, ReportVerdict};

use super::*;
use crate::test_support::{fake_client, json};

/// The real response to `addLabelIds:["SPAM"]` — note `INBOX` is gone although it was
/// never in `removeLabelIds`.
const SPAM: &str = include_str!("../tests/fixtures/mail/modify_spam.json");
/// The real response to `removeLabelIds:["SPAM"], addLabelIds:["INBOX"]`.
const NOT_SPAM: &str = include_str!("../tests/fixtures/mail/modify_not_spam.json");

fn target() -> ProviderKey {
    ProviderKey::new("message-1").unwrap()
}

fn report(verdict: ReportVerdict, destination: &str) -> MessageReport {
    MessageReport::new(target(), verdict, MailboxId::try_from(destination).unwrap())
}

#[test]
fn a_junk_report_adds_spam_and_nothing_else() {
    // The server clears the place labels itself (captured in `modify_spam.json`), so
    // there is no move for the adapter to make.
    let r1 = report(ReportVerdict::Junk, "SPAM");
    let (add, remove) = label_delta(&r1).unwrap();
    assert_eq!(add, vec!["SPAM"]);
    assert!(remove.is_empty(), "{remove:?}");
}

#[test]
fn a_junk_report_trains_gmail_even_if_the_host_resolved_junk_to_something_else() {
    // `SPAM` is what the filter reads; the destination is not substituted for it.
    let r2 = report(ReportVerdict::Junk, "Label_99");
    let (add, _remove) = label_delta(&r2).unwrap();
    assert_eq!(add, vec!["SPAM"]);
}

#[test]
fn a_not_junk_report_must_add_the_destination_or_the_message_vanishes() {
    // Removing SPAM alone leaves the message in no place label at all — verified live.
    // This test is the guard against re-introducing that.
    let r3 = report(ReportVerdict::NotJunk, "INBOX");
    let (add, remove) = label_delta(&r3).unwrap();
    assert_eq!(remove, vec!["SPAM"]);
    assert_eq!(
        add,
        vec!["INBOX"],
        "not-junk must re-file, not just unlabel"
    );
}

#[test]
fn phishing_is_refused_here_not_quietly_filed_as_junk() {
    let r4 = report(ReportVerdict::Phishing, "SPAM");
    let err = label_delta(&r4).expect_err("Gmail has no phishing verdict");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(err.detail().contains("phishing"), "{}", err.detail());
}

#[tokio::test]
async fn a_junk_report_posts_modify_and_returns_the_unchanged_key() {
    let client = fake_client(vec![("/modify", json(SPAM))]);
    let receipt = report_message(&client, &report(ReportVerdict::Junk, "SPAM"))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, target());
}

#[tokio::test]
async fn a_not_junk_report_posts_modify_and_returns_the_unchanged_key() {
    let client = fake_client(vec![("/modify", json(NOT_SPAM))]);
    let receipt = report_message(&client, &report(ReportVerdict::NotJunk, "INBOX"))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, target());
}

#[test]
fn the_captured_responses_pin_the_two_behaviours_the_mapping_relies_on() {
    // These assertions are about the *server*, not our code: if a re-capture ever shows
    // different labels, the mapping above is wrong and these fail first.
    let spam: serde_json::Value = serde_json::from_str(SPAM).unwrap();
    let labels: Vec<&str> = spam["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(labels.contains(&"SPAM"));
    assert!(
        !labels.contains(&"INBOX"),
        "adding SPAM is expected to clear INBOX server-side: {labels:?}"
    );

    let not_spam: serde_json::Value = serde_json::from_str(NOT_SPAM).unwrap();
    let labels: Vec<&str> = not_spam["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(labels.contains(&"INBOX"), "{labels:?}");
    assert!(!labels.contains(&"SPAM"), "{labels:?}");
}
