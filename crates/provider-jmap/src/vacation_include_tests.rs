//! The reply while the user's rules run, offline: written with the server's flag off, switched by
//! the include, guarded across its two requests, and what a write that lands halfway says. The
//! answers are Stalwart's own (`tests/fixtures/`); `tests/live_rules_reply.rs` and
//! `tests/live_vacation.rs` prove it by delivering mail.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    vacation::{AutoReply, RulesEffect, VacationResponse},
    version::VacationVersion,
};
use engine_provider::{Vacation, VacationPrecondition};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    provider::provider_test_support::{FakeExecutor, account, fixture, sieve_listing},
    request::capability,
};

fn session() -> Value {
    json!({
        "capabilities": { capability::CORE: {}, capability::MAIL: {}, capability::SIEVE: {},
                          capability::VACATION_RESPONSE: {} },
        "primaryAccounts": { capability::MAIL: "c", capability::SIEVE: "c",
                             capability::VACATION_RESPONSE: "c" },
        "accounts": { "c": { "accountCapabilities": {
            capability::MAIL: {},
            capability::SIEVE: { "sieveExtensions": ["fileinto", "include", "vacation"] },
            capability::VACATION_RESPONSE: {}
        } } },
        "apiUrl": "https://mail.test.local/jmap/",
        "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "https://mail.test.local/upload/{accountId}/"
    })
}

fn over(responses: Vec<Value>, script: &str) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(
        FakeExecutor::from_session(&session(), responses)
            .with_download_body(script.as_bytes())
            .with_upload_blob_ids(["up1"]),
    );
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

/// Carol's scripts with the rules running and the reply's script beside them, and the reply's
/// state, one counter with theirs (captured).
fn rules_running() -> Value {
    sieve_listing()
}

const WITHOUT: &str = "keep;\r\n";
const WITH: &str =
    "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\nkeep;\r\n";

fn away() -> VacationResponse {
    VacationResponse::default()
        .turned_on()
        .with_reply(AutoReply::default().with_subject("Away").with_text("Away."))
}

fn read_at(state: &str) -> VacationResponse {
    VacationResponse::default().with_version(VacationVersion::new(state))
}

#[tokio::test]
async fn on_while_the_rules_run_writes_the_words_off_and_includes_the_reply() {
    let (provider, exec) = over(
        vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
            fixture("sieve_set_update_rules_response.json"),
        ],
        WITHOUT,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert_eq!(receipt.version, Some(VacationVersion::new("stmaq")));
    let requests = exec.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let reply = &requests[1]["methodCalls"][0][1];
    assert_eq!(reply["ifInState"], "steaq");
    assert_eq!(
        reply["update"]["singleton"]["isEnabled"], false,
        "the server's flag stays off"
    );
    assert_eq!(reply["update"]["singleton"]["subject"], "Away");
    let toggle = &requests[2]["methodCalls"][0][1];
    assert_eq!(
        toggle,
        &json!({ "accountId": "c", "ifInState": "stiaq", "update": { "bp": { "blobId": "up1" } } }),
        "guarded by the state the first write left"
    );
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert_eq!(uploaded, WITH);
}

#[tokio::test]
async fn off_while_the_rules_run_takes_the_include_out_and_unchanged_sends_one_write() {
    let (provider, exec) = over(
        vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
            fixture("sieve_set_update_rules_response.json"),
        ],
        WITH,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away().turned_off(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert_eq!(uploaded, "require [\"include\"];\r\nkeep;\r\n");

    // On, and already included: the words alone are written.
    let (provider, exec) = over(
        vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
        ],
        WITH,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("stiaq")));
    assert_eq!(exec.request_count(), 2);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_stale_read_is_a_conflict_before_anything_is_written() {
    let (provider, exec) = over(vec![rules_running()], WITHOUT);
    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("older")),
            &away(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(exec.request_count(), 1, "the listing alone");
}

#[tokio::test]
async fn words_stored_and_the_switch_refused_or_lost_ask_for_a_read_before_writing_again() {
    for second in [Some(fixture("sieve_set_stale_response.json")), None] {
        let mut responses = vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
        ];
        responses.extend(second);
        let (provider, _) = over(responses, WITHOUT);
        let err = provider
            .set_vacation(
                &account(),
                VacationPrecondition::IfUnchanged(&read_at("steaq")),
                &away(),
            )
            .await
            .unwrap_err();
        assert!(err.requires_confirmation(), "{err:?}");
        assert!(err.to_string().contains("text was stored"), "{err}");
    }
    // The words refused: nothing was written, and the refusal keeps its class.
    let refused = json!({ "methodResponses": [["VacationResponse/set", { "notUpdated": {
        "singleton": { "type": "invalidProperties", "properties": ["subject"] }
    } }, "0"]] });
    let (provider, _) = over(vec![rules_running(), refused], WITHOUT);
    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!err.requires_confirmation());
}

#[tokio::test]
async fn a_reply_the_running_rules_include_reads_as_on_though_its_flag_says_off() {
    let doc = |listing: Value| {
        json!({ "methodResponses": [
            ["VacationResponse/get", { "accountId": "e", "state": "steaq", "list": [{
                "id": "singleton", "isEnabled": false, "fromDate": null, "toDate": null,
                "subject": "Away", "textBody": "Away.", "htmlBody": null
            }], "notFound": [] }, "0"],
            ["SieveScript/get", listing["methodResponses"][0][1].clone(), "1"]
        ] })
    };
    let (provider, _) = over(vec![doc(rules_running())], WITH);
    assert!(provider.read_vacation(&account()).await.unwrap().enabled);
    let (provider, _) = over(vec![doc(rules_running())], WITHOUT);
    assert!(!provider.read_vacation(&account()).await.unwrap().enabled);
}

#[tokio::test]
async fn a_running_script_that_cannot_take_the_include_falls_back_to_the_flag_never_refuses() {
    // It needs no include to switch the reply off: the words alone.
    let (provider, exec) = over(
        vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
        ],
        "if true {",
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away().turned_off(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert_eq!(exec.request_count(), 2);
    // It would need the include to switch the reply on: the server's own flag, as without rules,
    // and the receipt says what that did to them.
    let (provider, exec) = over(
        vec![
            rules_running(),
            fixture("vacation_set_deactivates_response.json"),
        ],
        "if true {",
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    let requests = exec.requests.lock().unwrap();
    assert_eq!(
        requests.last().unwrap()["methodCalls"][1][1]["update"]["singleton"]["isEnabled"],
        true
    );
}

#[tokio::test]
async fn an_unguarded_write_does_not_toggle_a_script_that_moved_after_its_listing() {
    // B2: the words land with no guard; the state they left from is not the listing's, so the
    // script the include was composed over may not be the user's latest. It is not written.
    let mut moved = fixture("vacation_set_kept_off_response.json");
    moved["methodResponses"][0][1]["oldState"] = json!("later");
    let (provider, exec) = over(vec![rules_running(), moved], WITHOUT);
    let err = provider
        .set_vacation(&account(), VacationPrecondition::Unconditional, &away())
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
    assert_eq!(exec.request_count(), 2, "no include written");
}

#[tokio::test]
async fn without_the_condition_the_reply_is_written_and_read_with_its_own_flag() {
    // B5: the reply's state is not the scripts' (or the account lists no `include`): the flag.
    let mut apart = rules_running();
    apart["methodResponses"][1][1]["state"] = json!("elsewhere");
    let (provider, exec) = over(
        vec![apart, fixture("vacation_set_deactivates_response.json")],
        WITHOUT,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sbi")),
            &away(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_read_falls_back_to_the_flag_where_the_rules_cannot_be_read_and_follows_the_include() {
    let doc = |state: &str| {
        json!({ "methodResponses": [
            ["VacationResponse/get", { "accountId": "e", "state": state, "list": [{
                "id": "singleton", "isEnabled": false, "fromDate": null, "toDate": null,
                "subject": "Away", "textBody": "Away.", "htmlBody": null
            }], "notFound": [] }, "0"],
            ["SieveScript/get", rules_running()["methodResponses"][0][1].clone(), "1"]
        ] })
    };
    // B4: the running script's blob cannot be downloaded.
    let exec = Arc::new(FakeExecutor::from_session(&session(), vec![doc("steaq")]));
    let provider = JmapProvider::with_executor(Box::new(exec));
    assert!(!provider.read_vacation(&account()).await.unwrap().enabled);
    // Nor decoded.
    let exec = Arc::new(
        FakeExecutor::from_session(&session(), vec![doc("steaq")]).with_download_body(b"\xff"),
    );
    let provider = JmapProvider::with_executor(Box::new(exec));
    assert!(!provider.read_vacation(&account()).await.unwrap().enabled);
    // The include is there and the reply's state is not the scripts': the server still runs the
    // reply through it at delivery, so it reads as on.
    let (provider, _) = over(vec![doc("elsewhere")], WITH);
    assert!(provider.read_vacation(&account()).await.unwrap().enabled);
}

#[tokio::test]
async fn a_switch_answered_with_neither_map_asks_for_a_read_too() {
    let unanswered = json!({ "methodResponses": [["SieveScript/set", { "newState": "x" }, "0"]] });
    let (provider, _) = over(
        vec![
            rules_running(),
            fixture("vacation_set_kept_off_response.json"),
            unanswered,
        ],
        WITHOUT,
    );
    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away(),
        )
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
}

/// The listing with the reply's state unread: its `VacationResponse/get` failed.
fn reply_state_unread() -> Value {
    let mut doc = rules_running();
    doc["methodResponses"][1] = json!(["error", { "type": "serverFail" }, "1"]);
    doc
}

#[tokio::test]
async fn off_takes_the_include_out_without_the_shared_counter() {
    // NEW-1: the reply's state could not be read; taking the include out needs only the script.
    let (provider, exec) = over(
        vec![
            reply_state_unread(),
            fixture("vacation_set_kept_off_response.json"),
            fixture("sieve_set_update_rules_response.json"),
        ],
        WITH,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away().turned_off(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert!(
        !uploaded.contains("include :"),
        "the include is out: {uploaded}"
    );
    let step_two = exec.requests.lock().unwrap()[2]["methodCalls"][0][1]["ifInState"].clone();
    assert_eq!(step_two, "stiaq", "the words' newState");

    // Turning it on with the state unread is the flag's, as without the condition.
    let (provider, exec) = over(
        vec![
            reply_state_unread(),
            fixture("vacation_set_deactivates_response.json"),
        ],
        WITHOUT,
    );
    let receipt = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("sbi")),
            &away(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn off_is_refused_before_anything_is_written_where_the_include_cannot_be_taken_out() {
    // NEW-1, the reviewer's probe: the running script includes the reply and cannot be
    // rewritten. The flag would answer "off" while the include kept the reply running.
    let probe = "require 5;\r\ninclude :personal :optional \"vacation\";\r\nkeep;";
    let (provider, exec) = over(vec![rules_running()], probe);
    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away().turned_off(),
        )
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert_eq!(exec.request_count(), 1, "the listing alone");
    // A script that cannot be read may include it: refused with the download's own class.
    let exec = Arc::new(FakeExecutor::from_session(
        &session(),
        vec![rules_running()],
    ));
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let err = provider
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&read_at("steaq")),
            &away().turned_off(),
        )
        .await
        .unwrap_err();
    assert!(!err.requires_confirmation(), "{err:?}");
    assert_eq!(exec.request_count(), 1);
}
