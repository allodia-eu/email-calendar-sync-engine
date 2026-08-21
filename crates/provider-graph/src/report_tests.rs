//! Offline tests for [`report_message`], driven against the captured real responses
//! and — for the request shape — the real reqwest transport at a capturing server.

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};
use engine_provider::{MessageReport, ReportVerdict};

use super::*;
use crate::{
    GraphClient,
    test_support::{capturing_server, tls},
};

/// The real 200 body Graph returned — a `reportMessageCommandResult`, **not** the
/// `message` object the documentation describes.
const REPORTED: &str = include_str!("../tests/fixtures/mail/message_reported.json");
/// The real 400 for an action outside the accepted three.
const BAD_ACTION: &str = include_str!("../tests/fixtures/error/report_bad_action.json");

fn target() -> ProviderKey {
    ProviderKey::new("message-write").unwrap()
}

fn report(verdict: ReportVerdict) -> MessageReport {
    MessageReport::new(
        target(),
        verdict,
        MailboxId::try_from("folder-junk").unwrap(),
    )
}

#[test]
fn only_the_three_actions_the_server_accepts_are_ever_sent() {
    // `unknown` and `unknownFutureValue` are documented enum members and are both
    // rejected with 400 by the live service, so they must be unreachable from here.
    assert_eq!(report_action(ReportVerdict::Junk), "junk");
    assert_eq!(report_action(ReportVerdict::NotJunk), "notJunk");
    assert_eq!(report_action(ReportVerdict::Phishing), "phish");
}

#[test]
fn the_captured_success_body_is_read_as_success() {
    let body: Value = serde_json::from_str(REPORTED).unwrap();
    assert_eq!(report_status(&body), Some("Success"));
    check_reported(Some(&body)).unwrap();
}

#[test]
fn a_200_that_does_not_say_success_is_not_a_success() {
    // The shape that would otherwise be a silent success: HTTP 200, status "Failed".
    let body = json!({ "properties": [{ "key": "Status", "value": "Failed" }] });
    let err = check_reported(Some(&body)).expect_err("a non-Success status must fail");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(err.detail().contains("Failed"), "{}", err.detail());
}

#[test]
fn an_absent_status_is_accepted_rather_than_invented_into_a_failure() {
    // The property bag is undocumented; a 2xx with nothing to read is the server
    // accepting the call, and refusing it here would fabricate errors.
    check_reported(None).unwrap();
    check_reported(Some(&json!({}))).unwrap();
    check_reported(Some(&json!({ "properties": [] }))).unwrap();
    check_reported(Some(
        &json!({ "properties": [{ "key": "Other", "value": "x" }] }),
    ))
    .unwrap();
}

#[test]
fn the_status_key_is_matched_case_insensitively() {
    let body = json!({ "properties": [{ "key": "status", "value": "success" }] });
    check_reported(Some(&body)).unwrap();
}

#[tokio::test]
async fn the_request_goes_to_the_beta_endpoint_with_the_action_and_the_move_flag() {
    // The offline fake ignores request bodies (`AGENTS.md`), so drive the REAL transport
    // at a capturing server to pin the bytes that actually go out.
    let (base, rx) = capturing_server("200 OK", REPORTED);
    let client = GraphClient::with_base("tok", base, tls()).unwrap();

    let receipt = report_message(&client, &report(ReportVerdict::Phishing))
        .await
        .unwrap();
    // The immutable id survives the server-side move, so the key is unchanged.
    assert_eq!(receipt.message_key, target());

    let req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        req.starts_with("POST /me/messages/message-write/reportMessage "),
        "{req}"
    );
    assert!(req.contains("\"ReportAction\":\"phish\""), "{req}");
    // Sent as `true` because the server moves the message whatever this says.
    assert!(req.contains("\"IsMessageMoveRequested\":true"), "{req}");
    // The immutable-id header is what keeps the receipt key resolvable after the move.
    assert!(req.contains("IdType=\"ImmutableId\""), "{req}");
}

#[tokio::test]
async fn a_rejected_action_surfaces_as_a_permanent_failure() {
    let (base, _rx) = capturing_server("400 Bad Request", BAD_ACTION);
    let client = GraphClient::with_base("tok", base, tls()).unwrap();

    let err = report_message(&client, &report(ReportVerdict::Junk))
        .await
        .expect_err("a 400 must not read as a delivered report");
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[test]
fn beta_url_switches_only_the_version_segment() {
    let client = GraphClient::connect("tok", tls()).unwrap();
    assert_eq!(
        client.beta_url("/messages/x/reportMessage"),
        "https://graph.microsoft.com/beta/me/messages/x/reportMessage"
    );
    // Every other call stays on v1.0.
    assert_eq!(
        client.url("/messages/x"),
        "https://graph.microsoft.com/v1.0/me/messages/x"
    );
}
