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
fn session(urns: &[&str]) -> Value {
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

fn over(session: &Value, responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::from_session(session, responses));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

/// A provider for an account with a reply and Sieve, answering `responses`.
fn with_sieve(responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    over(
        &session(&[capability::VACATION_RESPONSE, capability::SIEVE]),
        responses,
    )
}

fn calls(exec: &FakeExecutor) -> Vec<(String, Value)> {
    let requests = exec.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one request");
    requests[0]["methodCalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| (call[0].as_str().unwrap().to_owned(), call[1].clone()))
        .collect()
}

fn using(exec: &FakeExecutor) -> Value {
    exec.requests.lock().unwrap()[0]["using"].clone()
}

fn away() -> VacationResponse {
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

fn read_at(state: &str) -> VacationResponse {
    VacationResponse::default().with_version(VacationVersion::new(state))
}

#[test]
fn an_account_has_a_reply_only_where_the_session_addresses_one() {
    let (with, _) = over(&session(&[capability::VACATION_RESPONSE]), vec![]);
    assert!(with.vacation().is_some());
    let (without, _) = over(&session(&[]), vec![]);
    assert!(without.vacation().is_none());

    // The server speaks it, but names no account for it.
    let mut unaddressed = session(&[capability::VACATION_RESPONSE]);
    unaddressed["primaryAccounts"]
        .as_object_mut()
        .unwrap()
        .remove(capability::VACATION_RESPONSE);
    let (unaddressed, _) = over(&unaddressed, vec![]);
    assert!(unaddressed.vacation().is_none());
}

#[test]
fn a_read_only_share_has_no_reply_and_a_group_share_has_its_own() {
    // Captured as alice: bob's read-only share lists no `vacationresponse`; the group lists it.
    let shared = fixture("session_shared_accounts.json");
    let bound = |handle| {
        JmapProvider::with_executor(Box::new(FakeExecutor::bound_to(
            &shared,
            Some(handle),
            vec![],
        )))
    };
    assert!(bound("d").vacation().is_none());
    assert!(bound("g").vacation().is_some());
}

#[tokio::test]
async fn a_read_carries_every_part_its_version_and_the_object_as_read() {
    let (provider, exec) = with_sieve(vec![fixture("vacation_get_response.json")]);

    let read = provider
        .vacation()
        .unwrap()
        .read_vacation(&account())
        .await
        .unwrap();

    assert_eq!(read.clone().with_raw(RawVacation::new("")), {
        away()
            .with_version(VacationVersion::new("sba"))
            .with_raw(RawVacation::new(""))
    });
    assert_eq!(read.audience, ReplyAudience::Everyone);
    let raw: Value = serde_json::from_str(read.raw.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(raw["id"], "singleton");
    assert_eq!(
        using(&exec),
        json!([capability::CORE, capability::VACATION_RESPONSE])
    );
    assert_eq!(
        calls(&exec),
        [(
            "VacationResponse/get".to_owned(),
            json!({ "accountId": "c", "ids": ["singleton"] })
        )]
    );
}

#[tokio::test]
async fn a_read_without_the_singleton_or_with_a_bad_property_fails_naming_no_value() {
    let answer = |object: Value| {
        json!({ "methodResponses": [
            ["VacationResponse/get", { "state": "s1", "list": [object], "notFound": [] }, "0"]
        ] })
    };
    for (object, says) in [
        (json!({ "id": "other", "isEnabled": true }), "no singleton"),
        (json!({ "id": "singleton" }), "without isEnabled"),
        (
            json!({ "id": "singleton", "isEnabled": true, "fromDate": "Tuesday at the clinic" }),
            "fromDate that is not a UTCDate",
        ),
        (
            json!({ "id": "singleton", "isEnabled": true, "toDate": 7 }),
            "toDate that is not a UTCDate",
        ),
    ] {
        let (provider, _) = with_sieve(vec![answer(object)]);
        let err = provider.read_vacation(&account()).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(err.detail().contains(says), "{}", err.detail());
        assert!(!err.detail().contains("clinic"), "{}", err.detail());
    }
}

#[tokio::test]
async fn a_write_sends_every_part_guarded_by_the_version_read_between_two_script_listings() {
    let (provider, exec) = with_sieve(vec![fixture("vacation_set_response.json")]);

    let receipt = provider
        .set_vacation(&account(), &read_at("sa2"), &away())
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
async fn turning_the_reply_on_over_the_user_s_own_script_reports_it_switched_off() {
    let (provider, _) = with_sieve(vec![fixture("vacation_set_deactivates_response.json")]);

    let receipt = provider
        .set_vacation(&account(), &read_at("sbi"), &away())
        .await
        .unwrap();

    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    assert_eq!(receipt.version, Some(VacationVersion::new("sbm")));
}

#[tokio::test]
async fn a_write_built_on_a_stale_read_is_a_conflict_that_names_no_content() {
    let (provider, _) = with_sieve(vec![fixture("vacation_set_stale_response.json")]);

    let err = provider
        .set_vacation(&account(), &read_at("sa2"), &away())
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(err.detail().contains("stateMismatch"), "{}", err.detail());
    assert!(!format!("{err:?}").contains("ninth"));
}

#[tokio::test]
async fn a_reply_built_rather_than_read_writes_unguarded_and_without_sieve_the_rules_are_unknown() {
    let answer = json!({ "methodResponses": [
        ["VacationResponse/set", { "oldState": "s1", "newState": "s2", "updated": { "singleton": null } }, "0"]
    ] });
    let (provider, exec) = over(&session(&[capability::VACATION_RESPONSE]), vec![answer]);

    let receipt = provider
        .set_vacation(
            &account(),
            &VacationResponse::default(),
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
            &read_at("s1"),
            &away().with_audience(ReplyAudience::Contacts),
        )
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::InvalidState);
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
    let (provider, _) = with_sieve(vec![refused, unanswered]);
    let vacation = provider.vacation().unwrap();

    let err = vacation
        .set_vacation(&account(), &read_at("s1"), &away())
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
        .set_vacation(&account(), &read_at("s1"), &away())
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");

    // The fake has nothing left: the request failed with no answer.
    let err = vacation
        .set_vacation(&account(), &read_at("s1"), &away())
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
    let (provider, _) = with_sieve(vec![answer]);

    let receipt = provider
        .set_vacation(&account(), &read_at("s1"), &away())
        .await
        .unwrap();

    assert_eq!(receipt.rules, RulesEffect::Unknown);
    assert_eq!(receipt.version, Some(VacationVersion::new("s2")));
}

#[test]
fn only_a_script_switched_off_while_the_reply_goes_on_is_the_user_s() {
    let scripts = |pairs: &[(&str, bool)]| -> BTreeMap<String, bool> {
        pairs
            .iter()
            .map(|(id, on)| ((*id).to_owned(), *on))
            .collect()
    };
    let own_on = scripts(&[("own", true), ("vacation", false)]);
    let reply_on = scripts(&[("own", false), ("vacation", true)]);

    assert_eq!(effect(&own_on, &reply_on, true), RulesEffect::Deactivated);
    // Turning the reply off takes only the reply's own script with it.
    assert_eq!(
        effect(&reply_on, &own_on.clone(), false),
        RulesEffect::Untouched
    );
    assert_eq!(
        effect(
            &reply_on,
            &scripts(&[("own", false), ("vacation", false)]),
            false
        ),
        RulesEffect::Untouched
    );
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
async fn a_client_without_a_reply_says_so_when_asked_anyway() {
    let (provider, exec) = over(&session(&[]), vec![]);
    let err = provider.read_vacation(&account()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(
        err.detail().contains("no out-of-office reply"),
        "{}",
        err.detail()
    );
    assert_eq!(exec.request_count(), 0);
    assert_eq!(provider.guard(), WriteGuard::Enforced);
}

#[tokio::test]
async fn a_jmap_reply_holds_a_window_and_a_subject_for_everyone_and_reads_off_as_off() {
    // As Stalwart answers a scratch account that never set one.
    let untouched = json!({ "methodResponses": [
        ["VacationResponse/get", { "state": "n", "list": [{
            "id": "singleton", "isEnabled": false, "fromDate": null, "toDate": null,
            "subject": null, "textBody": null, "htmlBody": null
        }], "notFound": [] }, "0"]
    ] });
    let (provider, _) = with_sieve(vec![untouched]);
    let vacation = provider.vacation().unwrap();

    assert_eq!(
        vacation.vocabulary(),
        VacationVocabulary::everyone().with_window().with_subject()
    );
    let read = vacation.read_vacation(&account()).await.unwrap();
    assert!(!read.enabled);
    assert!(read.window.is_open());
    assert!(read.reply.is_empty());
    assert_eq!(read.version, Some(VacationVersion::new("n")));
}
