//! Reading the out-of-office reply offline: which sessions have one, and how each captured or
//! malformed answer reads. The writes are in `vacation_tests.rs`.

use engine_core::{
    error::FailureClass,
    vacation::{ReplyAudience, WindowSupport},
};
use engine_provider::Provider;
use serde_json::{Value, json};

use super::{tests::*, *};
use crate::provider::provider_test_support::{FakeExecutor, account, fixture};

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

    // An account for it, on a server that does not list it.
    let mut unlisted = session(&[capability::VACATION_RESPONSE]);
    unlisted["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove(capability::VACATION_RESPONSE);
    let (unlisted, exec) = over(&unlisted, vec![]);
    assert!(unlisted.vacation().is_none());
    assert_eq!(
        exec.session()
            .extension_account(capability::VACATION_RESPONSE),
        None
    );
    let (_, exec) = over(&session(&[capability::SIEVE]), vec![]);
    assert_eq!(
        exec.session()
            .extension_account(capability::SIEVE)
            .map(JmapAccountId::as_str),
        Some("c")
    );
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
        json!([
            capability::CORE,
            capability::VACATION_RESPONSE,
            capability::SIEVE
        ])
    );
    // The scripts are listed in the same request, to see whether the user's rules run the
    // reply; the captured answer has no listing, which reads as the flag says.
    assert_eq!(
        calls(&exec),
        [
            (
                "VacationResponse/get".to_owned(),
                json!({ "accountId": "c", "ids": ["singleton"] })
            ),
            (
                "SieveScript/get".to_owned(),
                json!({ "accountId": "c", "ids": null, "properties": ["name", "isActive", "blobId"] })
            )
        ]
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
    assert_eq!(provider.vacation_write_guard(), WriteGuard::Enforced);
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
        vacation.vocabulary(&account()).await.unwrap(),
        VacationVocabulary::everyone()
            .with_window(WindowSupport::EitherEnd)
            .with_subject()
            .with_text_and_html()
    );
    let read = vacation.read_vacation(&account()).await.unwrap();
    assert!(!read.enabled);
    assert!(read.window.is_open());
    assert!(read.reply.is_empty());
    assert_eq!(read.version, Some(VacationVersion::new("n")));
}

#[tokio::test]
async fn a_read_without_a_string_state_fails_rather_than_lose_its_guard() {
    let answer = |state: Option<Value>| {
        let mut result = json!({ "list": [{ "id": "singleton", "isEnabled": true }] });
        if let Some(state) = state {
            result["state"] = state;
        }
        json!({ "methodResponses": [["VacationResponse/get", result, "0"]] })
    };
    for state in [None, Some(json!(7)), Some(Value::Null)] {
        let (provider, _) = with_sieve(vec![answer(state.clone())]);
        let err = provider.read_vacation(&account()).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent, "{state:?}");
        assert!(err.detail().contains("without a state"), "{}", err.detail());
    }
}

#[tokio::test]
async fn a_text_property_that_is_not_a_string_fails_as_a_date_does_naming_no_value() {
    for property in ["subject", "textBody", "htmlBody"] {
        let mut object = json!({ "id": "singleton", "isEnabled": true });
        object[property] = json!({ "secret": "clinic" });
        let answer = json!({ "methodResponses": [
            ["VacationResponse/get", { "state": "s1", "list": [object] }, "0"]
        ] });
        let (provider, _) = with_sieve(vec![answer]);
        let err = provider.read_vacation(&account()).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(
            err.detail()
                .contains(&format!("{property} that is not a string")),
            "{}",
            err.detail()
        );
        assert!(!err.detail().contains("clinic"));
    }
}
