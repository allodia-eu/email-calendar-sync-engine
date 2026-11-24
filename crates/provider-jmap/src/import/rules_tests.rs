//! Rules and the reply imported offline: the folders asked of the server, the rules appended after
//! the account's own through the edit's one write path, and the reply's guards. The listing and
//! the store are Stalwart's own captured answers (`tests/fixtures/sieve_*.json`);
//! `tests/live_rules_import.rs` proves the server takes these requests.

use std::sync::Arc;

use engine_core::{
    mail::{Keyword, SystemKeyword},
    rules::{
        Kept, LossReason, MailboxPath, Match, RuleAddress, RuleName, RulePart, RulesActivation,
        RulesStored, TextOp, TextTest,
    },
    vacation::AutoReply,
    version::RuleSetVersion,
};
use serde_json::{Value, json};

use super::*;
use crate::provider::provider_test_support::{FakeExecutor, account, fixture, sieve_listing};

/// A session for account `c` with Sieve (filing by folder id among its extensions), a reply,
/// and the blob URLs.
fn session(reply: bool) -> Value {
    let sieve = json!({
        "sieveExtensions": ["fileinto", "imap4flags", "include", "copy", "mailboxid", "vacation"],
        "maxNumberRedirects": 1, "maxSizeScript": 102_400
    });
    let mut doc = json!({
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
    });
    if !reply {
        doc["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove(capability::VACATION_RESPONSE);
    }
    doc
}

fn over(responses: Vec<Value>, script: &str) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(
        FakeExecutor::from_session(&session(true), responses)
            .with_download_body(script.as_bytes())
            .with_upload_blob_ids(["up1"]),
    );
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

/// The account's own script: the reply included first, then one rule of the user's.
const OWN: &str = "require [\"fileinto\",\"include\"];\r\ninclude :personal :optional \
                   \"vacation\";\r\n# rule:[Mine]\r\nif header :contains \"subject\" \"mine\"\r\n\
                   {\r\n\tfileinto \"Mine\";\r\n}\r\n";

/// `Mailbox/get` for the ids the rules name, as Stalwart answered it for `carol@`'s inbox (`a`)
/// and an id it does not have (`AAMkunmapped`).
fn mailboxes() -> Value {
    fixture("mailbox_get_rule_folders_response.json")
}

fn named(name: &str, actions: Vec<Action>) -> MailRule {
    let test = TextTest::new(TextOp::Contains, vec![name.to_owned()]).unwrap();
    let when = Match::all(vec![engine_core::rules::Condition::Header {
        field: engine_core::rules::HeaderField::Subject,
        test,
    }]);
    MailRule::typed(when, actions).named(RuleName::new(name).unwrap())
}

fn into_id(id: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy: false,
    }
}

fn imported() -> Vec<MailRule> {
    let seen = Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into());
    vec![
        named(
            "Forward",
            vec![Action::Redirect {
                to: vec![RuleAddress::new("away@example.com").unwrap()],
                copy: false,
            }],
        ),
        named("Finance", vec![into_id("a")]),
        named("Lost", vec![into_id("AAMkunmapped"), seen]),
        named(
            "Path",
            vec![Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Archive").unwrap()),
                copy: false,
            }],
        ),
    ]
}

#[tokio::test]
async fn rules_are_appended_after_the_accounts_own_byte_for_byte_and_the_reply_stays_included() {
    let (provider, exec) = over(
        vec![
            mailboxes(),
            sieve_listing(),
            sieve_listing(),
            fixture("sieve_set_update_rules_response.json"),
        ],
        OWN,
    );
    let importer = provider.rules_importer().expect("an account with rules");
    let receipt = importer
        .import_rules(&account(), &imported(), ForwardingRules::Disabled)
        .await
        .unwrap();

    let kept: Vec<Kept> = receipt
        .outcomes
        .iter()
        .map(|outcome| outcome.kept)
        .collect();
    assert_eq!(
        kept,
        [Kept::Disabled, Kept::Whole, Kept::Reduced, Kept::Whole]
    );
    let lost: Vec<(RulePart, LossReason)> = receipt.outcomes[2]
        .losses
        .iter()
        .map(|loss| (loss.part, loss.reason))
        .collect();
    assert_eq!(lost, [(RulePart::Action(0), LossReason::MissingMailbox)]);
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(receipt.activation, RulesActivation::Running);
    assert_eq!(receipt.version, Some(RuleSetVersion::new("stmaq")));

    let requests = exec.requests.lock().unwrap();
    assert_eq!(
        requests[0]["methodCalls"][0],
        json!(["Mailbox/get", {
            "accountId": "c", "ids": ["AAMkunmapped", "a"], "properties": ["id"]
        }, "0"])
    );
    assert_eq!(
        requests.last().unwrap()["methodCalls"][0],
        json!(["SieveScript/set", {
            "accountId": "c", "ifInState": "steaq",
            "update": { "bp": { "blobId": "up1" } }
        }, "0"])
    );
    let uploads = exec.uploads.lock().unwrap();
    let text = String::from_utf8_lossy(&uploads[0].2);
    // The account's own rule and the include are copied as they were; only `require` grows.
    let own_rules = &OWN[OWN.find("include :personal").unwrap()..];
    let at = text
        .find(own_rules)
        .expect("the account's own rules, byte for byte");
    let added = &text[at + own_rules.len()..];
    assert!(text.starts_with("require ["), "{text}");
    assert!(
        added.starts_with("# rule:[Forward]\r\nif false # header :contains"),
        "the redirect is appended, disabled: {added}"
    );
    assert!(added.contains(":mailboxid \"a\""), "{added}");
    assert!(!added.contains("AAMkunmapped"), "{added}");
    assert!(added.contains("fileinto \"Archive\""), "{added}");
}

#[tokio::test]
async fn no_folder_named_by_id_asks_the_server_nothing() {
    let (provider, exec) = over(vec![sieve_listing()], OWN);
    let receipt = provider
        .import_rules(&account(), &[], ForwardingRules::Disabled)
        .await
        .unwrap();
    assert!(receipt.outcomes.is_empty(), "nothing handed in");
    let requests = exec.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the read alone: nothing to add, nothing written"
    );
    assert_eq!(requests[0]["methodCalls"][0][0], "SieveScript/get");
}

#[tokio::test]
async fn a_mailbox_answer_without_a_list_is_refused_before_anything_is_written() {
    let broken = json!({ "methodResponses": [["Mailbox/get", { "accountId": "c" }, "0"]],
                         "sessionState": "s" });
    let (provider, exec) = over(vec![broken], OWN);
    let err = provider
        .import_rules(&account(), &imported(), ForwardingRules::Disabled)
        .await
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert_eq!(exec.uploads.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn the_reply_is_imported_where_the_account_holds_none_and_refused_where_it_holds_one() {
    // A reply of the account's own, read through `VacationResponse/get`: refused, nothing set.
    let (provider, exec) = over(vec![fixture("vacation_get_response.json")], "");
    let reply = VacationResponse::default()
        .turned_on()
        .with_reply(AutoReply::default().with_text("Away"));
    let err = provider
        .import_vacation(&account(), &reply)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(
        exec.requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["methodCalls"][0][0] != "VacationResponse/set"),
        "nothing written"
    );
}

#[tokio::test]
async fn an_account_without_a_reply_takes_no_reply_import() {
    let exec = FakeExecutor::from_session(&session(false), Vec::new());
    let provider = JmapProvider::with_executor(Box::new(exec));
    let reply = VacationResponse::default().with_reply(AutoReply::default().with_text("Away"));
    let err = provider
        .import_vacation(&account(), &reply)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable());
    assert!(
        provider.rules_importer().is_some(),
        "the rules still import"
    );
}

#[test]
fn an_account_without_sieve_imports_no_rules() {
    let mut without = session(true);
    without["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove(capability::SIEVE);
    let exec = FakeExecutor::from_session(&without, Vec::new());
    assert!(
        JmapProvider::with_executor(Box::new(exec))
            .rules_importer()
            .is_none()
    );
}
