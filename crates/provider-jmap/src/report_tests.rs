use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};
use engine_provider::{MessageReport, ReportVerdict};
use serde_json::Value;

use super::*;

/// The real `Email/set` response the harness returned for a combined keyword + move
/// report (`tests/fixtures/email_set_report_response.json`, captured live).
const REPORT_RESPONSE: &str = include_str!("../tests/fixtures/email_set_report_response.json");
/// The real `SetError` the harness returned for an id it does not know.
const NOT_FOUND_RESPONSE: &str =
    include_str!("../tests/fixtures/email_set_report_notfound_response.json");

fn report(verdict: ReportVerdict) -> MessageReport {
    MessageReport::new(
        ProviderKey::new("f2aaaabp").unwrap(),
        verdict,
        MailboxId::try_from("c").unwrap(),
    )
}

/// The `args` of the single `Email/set` the adapter would send.
fn patch_for(verdict: ReportVerdict) -> Value {
    patch(&report(verdict))
}

#[test]
fn a_junk_report_sets_the_keyword_clears_its_opposite_and_files_the_message() {
    let patch = patch_for(ReportVerdict::Junk);
    assert_eq!(patch["keywords/$junk"], Value::Bool(true));
    // The contradiction is cleared in the same patch, never left for the server to
    // resolve arbitrarily.
    assert_eq!(patch["keywords/$notjunk"], Value::Null);
    assert_eq!(patch["mailboxIds"]["c"], Value::Bool(true));
}

#[test]
fn a_not_junk_report_clears_both_accusations() {
    let patch = patch_for(ReportVerdict::NotJunk);
    assert_eq!(patch["keywords/$notjunk"], Value::Bool(true));
    assert_eq!(patch["keywords/$junk"], Value::Null);
    // Vouching for a message must not leave the stronger claim standing.
    assert_eq!(patch["keywords/$phishing"], Value::Null);
}

#[test]
fn phishing_is_its_own_keyword_not_junk_under_another_name() {
    let patch = patch_for(ReportVerdict::Phishing);
    assert_eq!(patch["keywords/$phishing"], Value::Bool(true));
    // `$junk` is *not* set: RFC 8621 treats the two as independent keywords, and a
    // client that quietly sets both is asserting something the user did not say.
    assert!(patch.get("keywords/$junk").is_none());
}

#[test]
fn the_move_rides_the_same_set_so_it_cannot_land_half_applied() {
    // One PatchObject carries both halves — the property this test pins is that the
    // keyword and the membership are in the *same* request, which is what makes the
    // report atomic and one round-trip (verified live against Stalwart).
    let patch = patch_for(ReportVerdict::Junk);
    let keys: Vec<&String> = patch.as_object().unwrap().keys().collect();
    assert!(keys.iter().any(|k| k.starts_with("keywords/")));
    assert!(keys.iter().any(|k| *k == "mailboxIds"));
}

#[test]
fn the_captured_success_response_is_accepted() {
    let doc: Value = serde_json::from_str(REPORT_RESPONSE).unwrap();
    let result = &doc["methodResponses"][0][1];
    check_set_result_for(result, "f2aaaabp", "updated", "notUpdated").unwrap();
}

#[test]
fn the_captured_set_error_becomes_a_conflict() {
    let doc: Value = serde_json::from_str(NOT_FOUND_RESPONSE).unwrap();
    let result = &doc["methodResponses"][0][1];
    let err = check_set_result_for(result, "zzzznotanid", "updated", "notUpdated")
        .expect_err("a notFound SetError must not read as success");
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}

#[test]
fn a_target_the_server_silently_dropped_is_never_a_false_success() {
    // Neither `updated` nor `notUpdated` mentions our id.
    let result: Value = serde_json::json!({ "accountId": "c", "updated": { "other": null } });
    let err = check_set_result_for(&result, "f2aaaabp", "updated", "notUpdated")
        .expect_err("an unacknowledged target must fail");
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}
