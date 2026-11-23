//! The out-of-office reply offline: who has one, what is sent, and how each captured answer
//! reads. The answers are Stalwart's own (`tests/fixtures/vacation_*.json`, captured as the
//! scratch account `carol@`); `tests/live_vacation.rs` proves the server takes these requests.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    vacation::{ReplyAudience, RulesEffect},
};
use engine_provider::Provider;
use serde_json::{Value, json};

use super::*;
use crate::provider::provider_test_support::{FakeExecutor, account, fixture};

/// A session for account `c` whose server lists `urns` and addresses each at `c`.
pub(super) fn session(urns: &[&str]) -> Value {
    let mut doc = json!({
        "capabilities": { capability::CORE: {}, capability::MAIL: {} },
        "primaryAccounts": { capability::MAIL: "c" },
        "accounts": { "c": { "accountCapabilities": { capability::MAIL: {} } } },
        "apiUrl": "https://mail.test.local/jmap/",
    });
    for urn in urns {
        doc["capabilities"][*urn] = json!({});
        doc["primaryAccounts"][*urn] = json!("c");
        doc["accounts"]["c"]["accountCapabilities"][*urn] = json!({});
    }
    doc
}

pub(super) fn over(session: &Value, responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::from_session(session, responses));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

/// A provider for an account with a reply and Sieve, answering `responses`.
pub(super) fn with_sieve(responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    over(
        &session(&[capability::VACATION_RESPONSE, capability::SIEVE]),
        responses,
    )
}

/// The calls of the last request: the write, after the listing that decides how to write it.
pub(super) fn calls(exec: &FakeExecutor) -> Vec<(String, Value)> {
    let requests = exec.requests.lock().unwrap();
    requests.last().expect("a request")["methodCalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| (call[0].as_str().unwrap().to_owned(), call[1].clone()))
        .collect()
}

pub(super) fn using(exec: &FakeExecutor) -> Value {
    exec.requests.lock().unwrap().last().expect("a request")["using"].clone()
}

/// The listing a write of the reply starts with, where none of the user's scripts runs: the
/// reply is then written with the server's own flag (`crate::vacation_include` says when not).
/// The shape of a captured `SieveScript/get` (`vacation_set_response.json`, its first call).
pub(super) fn no_rules_running() -> Value {
    json!({ "methodResponses": [
        ["SieveScript/get", { "accountId": "e", "state": "s0", "list": [], "notFound": [] }, "0"]
    ] })
}

pub(super) fn away() -> VacationResponse {
    VacationResponse::default()
        .turned_on()
        .with_window(
            VacationWindow::default()
                .with_start("2030-01-02T09:00:00Z".parse().unwrap())
                .with_end("2030-01-09T17:00:00Z".parse().unwrap()),
        )
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("I am away until the ninth.")
                .with_html("<p>I am away until the <b>ninth</b>.</p>"),
        )
}

pub(super) fn read_at(state: &str) -> VacationResponse {
    VacationResponse::default().with_version(VacationVersion::new(state))
}

#[tokio::test]
async fn a_write_sends_every_part_guarded_by_the_version_read_between_two_script_listings() {
    let (provider, exec) = with_sieve(vec![
        no_rules_running(),
        fixture("vacation_set_response.json"),
    ]);

    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sa2")),
            &away(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.version, Some(VacationVersion::new("sba")));
    assert_eq!(
        receipt.rules,
        RulesEffect::Untouched,
        "no script ran before"
    );
    assert_eq!(
        using(&exec),
        json!([
            capability::CORE,
            capability::VACATION_RESPONSE,
            capability::SIEVE
        ])
    );
    let scripts = json!({ "accountId": "c", "ids": null, "properties": ["isActive"] });
    assert_eq!(
        calls(&exec),
        [
            ("SieveScript/get".to_owned(), scripts.clone()),
            (
                "VacationResponse/set".to_owned(),
                json!({
                    "accountId": "c",
                    "ifInState": "sa2",
                    "update": { "singleton": {
                        "isEnabled": true,
                        "fromDate": "2030-01-02T09:00:00Z",
                        "toDate": "2030-01-09T17:00:00Z",
                        "subject": "Away",
                        "textBody": "I am away until the ninth.",
                        "htmlBody": "<p>I am away until the <b>ninth</b>.</p>",
                    } },
                })
            ),
            ("SieveScript/get".to_owned(), scripts),
        ]
    );
}

#[tokio::test]
async fn the_user_s_script_is_reported_switched_off_by_the_reply_and_still_off_after_it() {
    let (provider, _) = with_sieve(vec![
        no_rules_running(),
        fixture("vacation_set_deactivates_response.json"),
        no_rules_running(),
        fixture("vacation_set_off_response.json"),
    ]);

    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sbi")),
            &away(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    assert_eq!(receipt.version, Some(VacationVersion::new("sbm")));

    let off = away().turned_off();
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sfa")),
            &off,
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::StillDeactivated);
}

#[tokio::test]
async fn a_write_built_on_a_stale_read_is_a_conflict_that_names_no_content() {
    // The server's description can echo what the request carried; here it does.
    let mut stale = fixture("vacation_set_stale_response.json");
    stale["methodResponses"][1][1]["description"] = json!("ifInState stale for: the ninth");
    let (provider, _) = with_sieve(vec![no_rules_running(), stale]);

    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sa2")),
            &away(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(err.detail().contains("stateMismatch"), "{}", err.detail());
    assert!(!err.to_string().contains("ninth"), "{err}");
    assert!(!format!("{err:?}").contains("ninth"), "{err:?}");
}

#[tokio::test]
async fn an_unconditional_write_sends_no_state_and_without_sieve_the_rules_are_unknown() {
    let answer = json!({ "methodResponses": [
        ["VacationResponse/set", { "oldState": "s1", "newState": "s2", "updated": { "singleton": null } }, "0"]
    ] });
    let (provider, exec) = over(&session(&[capability::VACATION_RESPONSE]), vec![answer]);

    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::Unconditional,
            &VacationResponse::default(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.rules, RulesEffect::Unknown);
    let sent = calls(&exec);
    assert_eq!(sent.len(), 1, "no script listing without Sieve");
    assert_eq!(sent[0].0, "VacationResponse/set");
    assert!(sent[0].1.get("ifInState").is_none());
    // A part left out is cleared, not kept from before.
    assert_eq!(
        sent[0].1["update"]["singleton"],
        json!({ "isEnabled": false, "fromDate": null, "toDate": null,
                "subject": null, "textBody": null, "htmlBody": null })
    );
    assert_eq!(
        using(&exec),
        json!([capability::CORE, capability::VACATION_RESPONSE])
    );
}

#[tokio::test]
async fn a_reply_the_account_cannot_hold_is_refused_before_anything_is_sent() {
    let (provider, exec) = with_sieve(vec![]);

    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("s1")),
            &away().with_audience(ReplyAudience::Contacts),
        )
        .await
        .unwrap_err();

    assert!(err.is_item_unavailable(), "{err:?}");
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn a_refusal_names_its_type_and_an_unanswered_or_lost_write_may_have_landed() {
    let refused = json!({ "methodResponses": [
        ["SieveScript/get", { "list": [] }, "0"],
        ["VacationResponse/set", { "notUpdated": { "singleton": {
            "type": "invalidProperties", "properties": ["toDate"], "description": "ninth"
        } } }, "1"],
        ["SieveScript/get", { "list": [] }, "2"]
    ] });
    let unanswered = json!({ "methodResponses": [
        ["SieveScript/get", { "list": [] }, "0"],
        ["VacationResponse/set", { "newState": "s2" }, "1"],
        ["SieveScript/get", { "list": [] }, "2"]
    ] });
    let (provider, _) = with_sieve(vec![
        no_rules_running(),
        refused,
        no_rules_running(),
        unanswered,
        no_rules_running(),
    ]);
    let vacation = provider.vacation().unwrap();

    let err = vacation
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("s1")),
            &away(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(
        err.detail().contains("invalidProperties"),
        "{}",
        err.detail()
    );
    assert!(err.detail().contains("toDate"), "{}", err.detail());
    assert!(!err.detail().contains("ninth"), "{}", err.detail());

    let err = vacation
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("s1")),
            &away(),
        )
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");

    // The fake has nothing left after the listing: the write failed with no answer.
    let err = vacation
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("s1")),
            &away(),
        )
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
}

#[tokio::test]
async fn a_script_listing_that_fails_leaves_the_rules_unknown_not_the_write() {
    let answer = json!({ "methodResponses": [
        ["error", { "type": "serverFail" }, "0"],
        ["VacationResponse/set", { "newState": "s2", "updated": { "singleton": null } }, "1"],
        ["SieveScript/get", { "list": [{ "id": "a", "isActive": true }] }, "2"]
    ] });
    let (provider, _) = with_sieve(vec![no_rules_running(), answer]);

    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("s1")),
            &away(),
        )
        .await
        .unwrap();

    assert_eq!(receipt.rules, RulesEffect::Unknown);
    assert_eq!(receipt.version, Some(VacationVersion::new("s2")));
}

#[test]
fn a_script_switched_off_for_the_reply_is_reported_going_and_staying_off() {
    let scripts = |pairs: &[(&str, bool)]| -> BTreeMap<String, bool> {
        pairs
            .iter()
            .map(|(id, on)| ((*id).to_owned(), *on))
            .collect()
    };
    let own_on = scripts(&[("own", true), ("vacation", false)]);
    let reply_on = scripts(&[("own", false), ("vacation", true)]);

    let none_on = scripts(&[("own", false), ("vacation", false)]);

    assert_eq!(effect(&own_on, &reply_on, true), RulesEffect::Deactivated);
    // Off again: the reply's script goes, and the user's stays off.
    assert_eq!(
        effect(&reply_on, &none_on, false),
        RulesEffect::StillDeactivated
    );
    // Off, with the user's script running again (another client switched it on): untouched.
    assert_eq!(effect(&reply_on, &own_on, false), RulesEffect::Untouched);
    // Off, on an account with no script but the reply's: nothing of the user's to restore.
    assert_eq!(
        effect(
            &scripts(&[("vacation", true)]),
            &scripts(&[("vacation", false)]),
            false
        ),
        RulesEffect::Untouched
    );
    // A reply that was off already, edited and left off: no transition, no report.
    assert_eq!(effect(&none_on, &none_on, false), RulesEffect::Untouched);
    // Editing a reply that is already on leaves its own script running.
    assert_eq!(effect(&reply_on, &reply_on, true), RulesEffect::Untouched);
    // A script gone after the write was not switched off.
    assert_eq!(
        effect(&own_on, &scripts(&[("vacation", true)]), true),
        RulesEffect::Untouched
    );
    // A listing that cannot be read is no listing.
    assert_eq!(active_by_id(&json!({ "list": [{ "id": "a" }] })), None);
}

#[tokio::test]
async fn a_guarded_write_of_a_reply_never_read_is_refused_before_anything_is_sent() {
    let (provider, exec) = with_sieve(vec![]);

    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&VacationResponse::default()),
            &away(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!err.is_item_unavailable() && !err.requires_confirmation());
    assert_eq!(exec.request_count(), 0, "never quietly sent unguarded");
}

#[tokio::test]
async fn a_script_listing_that_does_not_bracket_the_write_leaves_the_rules_unknown() {
    // Stalwart shows one counter: the listing before is at the write's oldState and the listing
    // after at its newState. Anything else means a change came between, so nothing is claimed.
    let moved = |call: usize, state: &str| {
        let mut doc = fixture("vacation_set_deactivates_response.json");
        doc["methodResponses"][call][1]["state"] = json!(state);
        doc
    };
    let mut no_state = fixture("vacation_set_deactivates_response.json");
    no_state["methodResponses"][2][1]
        .as_object_mut()
        .unwrap()
        .remove("state");
    for answer in [moved(0, "sbh"), moved(2, "sbn"), no_state] {
        let (provider, _) = with_sieve(vec![no_rules_running(), answer]);
        let receipt = provider
            .set_vacation(
                &account(),
                VacationPrecondition::IfUnchanged(&read_at("sbi")),
                &away(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.rules, RulesEffect::Unknown);
        assert_eq!(receipt.version, Some(VacationVersion::new("sbm")));
    }
}

#[tokio::test]
async fn a_reply_with_parts_left_out_clears_them_and_reads_back_without_them() {
    let (provider, exec) = with_sieve(vec![
        no_rules_running(),
        fixture("vacation_set_clears_response.json"),
        fixture("vacation_get_cleared_response.json"),
    ]);
    let start = "2030-02-01T08:00:00Z".parse().unwrap();
    let cleared = VacationResponse::default()
        .turned_on()
        .with_window(VacationWindow::default().with_start(start))
        .with_reply(AutoReply::default().with_text("Away for a while."));

    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sfq")),
            &cleared,
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.rules,
        RulesEffect::Untouched,
        "the reply's own script stays on"
    );
    let sent = exec.requests.lock().unwrap()[1]["methodCalls"][1][1]["update"]["singleton"].clone();
    assert_eq!(
        sent,
        json!({ "isEnabled": true, "fromDate": "2030-02-01T08:00:00Z", "toDate": null,
                "subject": null, "textBody": "Away for a while.", "htmlBody": null })
    );

    let read = provider.read_vacation(&account()).await.unwrap();
    assert_eq!(read.version, receipt.version);
    assert_eq!(
        (read.window, read.reply.clone()),
        (cleared.window, cleared.reply)
    );
}
