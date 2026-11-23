//! The rules offline: who has them, what is sent, and how each captured answer reads. The
//! answers are Stalwart's own (`tests/fixtures/sieve_*.json`, captured as the scratch account
//! `carol@`); `tests/live_rules.rs` and `tests/live_rules_reply.rs` prove the server takes these
//! requests.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    rules::{Action, Kept, MailRule, Match, ReplyEffect, RuleSet, RulesActivation, RulesStored},
    version::RuleSetVersion,
};
use engine_provider::{MailRules, Provider};
use serde_json::{Value, json};

use super::*;
use crate::provider::provider_test_support::{FakeExecutor, account, fixture, sieve_listing};

/// A session for account `c` with Sieve (Stalwart's extensions and limits), a reply, and the
/// blob URLs.
fn session(max_size: u64) -> Value {
    let sieve = json!({
        "sieveExtensions": ["fileinto", "imap4flags", "include", "regex", "copy", "vacation"],
        "maxNumberRedirects": 1, "maxSizeScript": max_size
    });
    json!({
        "capabilities": { capability::CORE: {}, capability::MAIL: {}, capability::SIEVE: {},
                          capability::VACATION_RESPONSE: {} },
        "primaryAccounts": { capability::MAIL: "c", capability::SIEVE: "c",
                             capability::VACATION_RESPONSE: "c" },
        "accounts": { "c": { "accountCapabilities": {
            capability::MAIL: {}, capability::SIEVE: sieve, capability::VACATION_RESPONSE: {}
        } } },
        "apiUrl": "https://mail.test.local/jmap/",
        "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "https://mail.test.local/upload/{accountId}/"
    })
}

fn over(responses: Vec<Value>, script: &str) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(
        FakeExecutor::from_session(&session(102_400), responses)
            .with_download_body(script.as_bytes())
            .with_upload_blob_ids(["up1"]),
    );
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

fn listing() -> Value {
    sieve_listing()
}

const RUNNING: &str = "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\n";

fn discard_all() -> MailRule {
    MailRule::typed(Match::default(), vec![Action::Discard])
}

#[test]
fn an_account_has_rules_only_where_the_session_addresses_sieve() {
    let (provider, _) = over(Vec::new(), "");
    assert!(provider.mail_rules().is_some());
    let mut without = session(1);
    without["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove(capability::SIEVE);
    let exec = FakeExecutor::from_session(&without, Vec::new());
    assert!(
        JmapProvider::with_executor(Box::new(exec))
            .mail_rules()
            .is_none()
    );
}

#[tokio::test]
async fn a_read_lists_the_scripts_and_reads_the_running_one_without_the_include() {
    let (provider, exec) = over(vec![listing()], RUNNING);
    let read = provider.read_rules(&account()).await.unwrap();
    assert_eq!(read.version, Some(RuleSetVersion::new("steaq")));
    assert_eq!(
        read.rules,
        Vec::new(),
        "the include is no rule of the user's"
    );
    assert_eq!(
        read.raw.as_ref().map(engine_core::raw::RawRules::as_str),
        Some(RUNNING)
    );
    assert_eq!(
        exec.download_urls.lock().unwrap()[0],
        "http://127.0.0.1:18080/download/c/ceatf79qwtdhlnli73023occ7i3l2foo3ljgy33l1qbv7dfahfxoobaff2ad1/script.sieve?accept=application%2Fsieve"
    );
    let vocabulary = provider.rule_vocabulary(&account()).await.unwrap();
    assert_eq!(vocabulary.max_redirects(), Some(1));
    assert_eq!(provider.rules_write_guard(), WriteGuard::Enforced);
}

#[tokio::test]
async fn a_write_uploads_the_script_and_stores_it_guarded_keeping_the_reply_included() {
    let (provider, exec) = over(
        vec![
            listing(),
            listing(),
            fixture("sieve_set_update_rules_response.json"),
        ],
        RUNNING,
    );
    let read = provider.read_rules(&account()).await.unwrap();
    let receipt = provider
        .replace_rules(&account(), &read, &[discard_all()])
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::Whole);
    assert_eq!(
        receipt.version.as_ref().map(RuleSetVersion::as_str),
        Some("stmaq")
    );
    let uploads = exec.uploads.lock().unwrap();
    let (url, media_type, bytes) = &uploads[0];
    assert_eq!(
        (url.as_str(), media_type.as_str()),
        ("http://127.0.0.1:18080/upload/c/", "application/sieve")
    );
    assert_eq!(
        String::from_utf8_lossy(bytes),
        "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\nif true\r\n{\r\n\tdiscard;\r\n}\r\n",
        "the reply stays included, first"
    );
    let requests = exec.requests.lock().unwrap();
    assert_eq!(
        requests.last().unwrap()["methodCalls"][0],
        json!(["SieveScript/set", {
            "accountId": "c", "ifInState": "steaq",
            "update": { "bp": { "blobId": "up1" } }
        }, "0"])
    );
}

#[tokio::test]
async fn a_first_write_creates_the_rules_script_and_activates_it() {
    let empty = json!({ "methodResponses": [
        ["SieveScript/get", { "accountId": "e", "state": "s1", "list": [], "notFound": [] }, "0"]
    ] });
    let (provider, exec) = over(
        vec![
            empty.clone(),
            empty,
            fixture("sieve_set_create_rules_response.json"),
        ],
        "",
    );
    let read = provider.read_rules(&account()).await.unwrap();
    let receipt = provider
        .replace_rules(&account(), &read, &[discard_all()])
        .await
        .unwrap();
    assert!(receipt.version.is_some());
    let requests = exec.requests.lock().unwrap();
    let set = &requests.last().unwrap()["methodCalls"][0][1];
    assert_eq!(set["create"]["r"]["name"], "rules");
    assert_eq!(set["onSuccessActivateScript"], "#r");
    assert_eq!(set["ifInState"], "s1");
}

#[tokio::test]
async fn a_stale_base_is_a_conflict_before_anything_is_sent_and_after_too() {
    let (provider, exec) = over(vec![listing()], RUNNING);
    let stale = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("older"));
    let err = provider
        .replace_rules(&account(), &stale, &[discard_all()])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0, "nothing uploaded");

    // The server's own refusal of a state that moved between the listing and the write.
    let (provider, _) = over(
        vec![listing(), fixture("sieve_set_stale_response.json")],
        RUNNING,
    );
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_script_the_server_refuses_is_item_unavailable_and_names_no_server_text() {
    let (provider, _) = over(
        vec![listing(), fixture("sieve_set_invalid_response.json")],
        RUNNING,
    );
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(err.detail().contains("invalidScript"), "{}", err.detail());
    assert!(!err.to_string().contains("parenthesis"), "{err}");
    assert!(!format!("{err:?}").contains("parenthesis"));
    // RFC 9661's own name for it reads the same, and so does a script too large; a full
    // account is the target's own state, which waiting does not clear; anything else keeps its
    // class.
    let refusal = |kind: &str| refused_script("bp", &json!({ "type": kind }));
    assert!(refusal("invalidSieve").is_item_unavailable());
    assert!(refusal("tooLarge").is_item_unavailable());
    let full = refusal("overQuota");
    assert_eq!(
        full.target_refusal(),
        Some(engine_provider::TargetRefusal::StorageFull)
    );
    assert!(!full.is_retryable());
    assert_eq!(refusal("forbidden").class(), FailureClass::Permanent);
}

#[tokio::test]
async fn rules_over_the_size_limit_are_refused_before_they_are_sent() {
    let exec = Arc::new(
        FakeExecutor::from_session(&session(10), vec![listing()])
            .with_download_body(RUNNING.as_bytes()),
    );
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_base_never_read_is_refused_and_a_lost_answer_may_have_landed() {
    let (provider, exec) = over(vec![], RUNNING);
    let err = provider
        .replace_rules(&account(), &RuleSet::new(Vec::new()), &[])
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(exec.request_count(), 0);

    // The write went out and no answer came back.
    let (provider, _) = over(vec![listing()], RUNNING);
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
    // An answer that names the script in neither map may have landed too.
    let (provider, _) = over(
        vec![
            listing(),
            json!({ "methodResponses": [["SieveScript/set", { "newState": "x" }, "0"]] }),
        ],
        RUNNING,
    );
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert!(err.requires_confirmation(), "{err:?}");
}

/// The listing with the rules script idle and the reply's script running.
fn reply_running_rules_idle() -> Value {
    let mut doc = listing();
    doc["methodResponses"][0][1]["list"][0]["isActive"] = json!(true);
    doc["methodResponses"][0][1]["list"][1]["isActive"] = json!(false);
    doc
}

#[tokio::test]
async fn rules_kept_in_an_idle_script_read_as_inactive_and_are_written_there_still_idle() {
    let (provider, exec) = over(
        vec![
            reply_running_rules_idle(),
            reply_running_rules_idle(),
            fixture("sieve_set_update_rules_response.json"),
        ],
        "keep;\r\n",
    );
    let read = provider.read_rules(&account()).await.unwrap();
    assert_eq!(read.activation, RulesActivation::Inactive);
    let receipt = provider
        .replace_rules(&account(), &read, &[discard_all()])
        .await
        .unwrap();
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(
        receipt.reply,
        ReplyEffect::Untouched,
        "the reply's own script still runs"
    );
    let requests = exec.requests.lock().unwrap();
    let set = &requests.last().unwrap()["methodCalls"][0][1];
    assert_eq!(
        set.get("onSuccessActivateScript"),
        None,
        "never switched on by a write"
    );
    assert_eq!(set["update"]["bp"]["blobId"], "up1");
}

#[tokio::test]
async fn an_account_with_no_rules_to_edit_gets_a_running_rules_script_that_keeps_the_reply() {
    // Only the reply's script: the write creates `rules`, makes it the active one, and includes
    // the reply's so it keeps running.
    let mut only_reply = reply_running_rules_idle();
    only_reply["methodResponses"][0][1]["list"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let (provider, exec) = over(
        vec![
            only_reply.clone(),
            fixture("sieve_set_create_rules_response.json"),
        ],
        "",
    );
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let receipt = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap();
    assert_eq!(receipt.stored, RulesStored::CreatedRunning);
    assert_eq!(receipt.reply, ReplyEffect::Untouched);
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert!(uploaded.contains("include :personal :optional \"vacation\";"));

    // Where the account does not list `include`, the new script cannot run the reply, and the
    // receipt says the write stopped it.
    let mut no_include = session(102_400);
    no_include["accounts"]["c"]["accountCapabilities"][capability::SIEVE]["sieveExtensions"] =
        json!(["fileinto"]);
    let exec = Arc::new(
        FakeExecutor::from_session(
            &no_include,
            vec![only_reply, fixture("sieve_set_create_rules_response.json")],
        )
        .with_upload_blob_ids(["up1"]),
    );
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let receipt = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap();
    assert_eq!(receipt.reply, ReplyEffect::Stopped);
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert!(!uploaded.contains("include"), "{uploaded}");
}

#[tokio::test]
async fn a_base_without_its_raw_form_is_written_over_the_server_s_script() {
    // B1: a versioned base with no raw form must not drop the include (and the reply) or the
    // rules it did not name: the script is read from the server.
    let server = "require [\"include\",\"vacation\"];\r\ninclude :personal :optional \"vacation\";\r\nvacation \"hand-written\";\r\n";
    let (provider, exec) = over(
        vec![listing(), fixture("sieve_set_update_rules_response.json")],
        server,
    );
    let mut base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    base.raw = None;
    let mut rules = sieve_scripts::user_rules(server);
    rules.push(discard_all());
    let _ = provider
        .replace_rules(&account(), &base, &rules)
        .await
        .unwrap();
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert!(
        uploaded.starts_with(server),
        "the include and the hand-written rule copied as they were: {uploaded:?}"
    );
    assert_eq!(
        exec.download_urls.lock().unwrap().len(),
        1,
        "read from the server"
    );
}

#[tokio::test]
async fn where_the_reply_and_the_scripts_do_not_share_a_state_the_reply_is_not_included() {
    // The condition: one counter for both types, in one response. A reply state other than the
    // scripts' leaves the include out, and the reply that ran through it stops.
    let mut apart = listing();
    apart["methodResponses"][1][1]["state"] = json!("elsewhere");
    let (provider, exec) = over(
        vec![apart, fixture("sieve_set_update_rules_response.json")],
        RUNNING,
    );
    let base = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("steaq"));
    let receipt = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap();
    assert_eq!(receipt.reply, ReplyEffect::Stopped);
    let uploaded = String::from_utf8(exec.uploads.lock().unwrap()[0].2.clone()).unwrap();
    assert!(!uploaded.contains("include :"), "{uploaded}");
}

#[tokio::test]
async fn a_script_that_cannot_be_edited_rule_by_rule_is_refused_and_no_sieve_is_said_so() {
    let (provider, exec) = over(vec![listing()], RUNNING);
    let base = RuleSet::new(Vec::new())
        .with_version(RuleSetVersion::new("steaq"))
        .with_raw(engine_core::raw::RawRules::new("if true {"));
    let err = provider
        .replace_rules(&account(), &base, &[discard_all()])
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);

    let mut without = session(1);
    without["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove(capability::SIEVE);
    let provider =
        JmapProvider::with_executor(Box::new(FakeExecutor::from_session(&without, Vec::new())));
    let err = MailRules::read_rules(&provider, &account())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}
