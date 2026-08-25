//! The report verb driven **end to end** through the fake executor.
//!
//! `report_tests` pins the patch and the result reading separately; a request that
//! assembled them wrongly would satisfy both. These sit beside `provider.rs` because
//! that is where the fake executor and the provider constructor live — the same reason
//! `calendar_write_tests` does.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, ProviderKey},
};
use engine_provider::{MessageReport, Provider, ReportVerdict};
use serde_json::Value;

use super::{provider_test_support::*, *};

/// The real `Email/set` response the harness returned for a combined keyword + move
/// report (captured live).
const REPORT_RESPONSE: &str = include_str!("../tests/fixtures/email_set_report_response.json");
/// The real `SetError` the harness returned for an id it does not know.
const NOT_FOUND_RESPONSE: &str =
    include_str!("../tests/fixtures/email_set_report_notfound_response.json");

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The report the captured fixtures answer: the harness's own message id, filed into
/// mailbox `c`.
fn report() -> MessageReport {
    MessageReport::new(
        ProviderKey::new("f2aaaabp").unwrap(),
        ReportVerdict::Junk,
        MailboxId::try_from("c").unwrap(),
    )
}

fn doc(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

#[tokio::test]
async fn the_whole_report_goes_out_as_one_email_set_carrying_both_halves() {
    let executor = std::sync::Arc::new(FakeExecutor::new(vec![doc(REPORT_RESPONSE)]));
    let p = JmapProvider::with_executor(Box::new(std::sync::Arc::clone(&executor)));

    let receipt = p
        .report_message(&account(), &report())
        .await
        .expect("the captured response is a success");
    assert_eq!(receipt.message_key, report().target);

    // Exactly one round trip, and the keyword and the filing rode it together — which is
    // what makes the report atomic rather than a message left flagged but unfiled.
    let sent = executor.requests.lock().unwrap();
    assert_eq!(sent.len(), 1, "one request: {sent:?}");
    let calls = &sent[0]["methodCalls"];
    assert_eq!(calls.as_array().map(Vec::len), Some(1), "{calls}");
    assert_eq!(calls[0][0], "Email/set");
    let patch = &calls[0][1]["update"]["f2aaaabp"];
    assert_eq!(patch["keywords/$junk"], Value::Bool(true));
    assert_eq!(patch["mailboxIds"]["c"], Value::Bool(true));
}

#[tokio::test]
async fn a_set_error_from_the_server_reaches_the_caller_as_a_conflict() {
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::new(vec![doc(NOT_FOUND_RESPONSE)])));
    let err = p
        .report_message(&account(), &report())
        .await
        .expect_err("a notFound SetError must not read as success");
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_read_only_account_does_not_advertise_reporting() {
    // Reporting rides the same gate as any other write, so a read-only account must not
    // offer it: reading the capability is how a host hides the action, rather than
    // finding out at the round trip.
    let session = serde_json::json!({
        "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "accounts": { "c": { "isReadOnly": true } },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(&session, vec![])));
    assert_eq!(p.connection_info().capabilities.mail_report(), None);
}
