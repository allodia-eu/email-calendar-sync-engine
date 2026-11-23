//! The request envelope and the response lookup, offline.

use serde_json::{Map, json};

use super::*;
use crate::{JmapError, response::Response};

#[test]
fn builds_batched_request_with_sequential_call_ids() {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let q = req.invoke("Email/query", json!({ "accountId": "c", "limit": 50 }));
    assert_eq!(q.as_str(), "0");
    let get_args = with_back_reference(
        json!({ "accountId": "c" }).as_object().unwrap().clone(),
        "ids",
        &ResultRef::new(&q, "Email/query", "/ids"),
    );
    let g = req.invoke("Email/get", get_args);
    assert_eq!(g.as_str(), "1");
    assert_ne!(q, g);

    let wire = req.to_json();
    assert_eq!(wire["using"], json!([capability::CORE, capability::MAIL]));
    assert_eq!(wire["methodCalls"][0][0], "Email/query");
    assert_eq!(wire["methodCalls"][0][2], "0");
    assert_eq!(wire["methodCalls"][1][0], "Email/get");
    // The back-reference is carried under "#ids".
    assert_eq!(
        wire["methodCalls"][1][1]["#ids"],
        json!({ "resultOf": "0", "name": "Email/query", "path": "/ids" })
    );
}

#[test]
fn a_request_declares_an_extension_named_at_run_time() {
    let sieve = String::from("urn:ietf:params:jmap:sieve");
    let req = Request::new([capability::CORE.to_owned(), sieve]);
    assert_eq!(
        req.to_json()["using"],
        json!(["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:sieve"])
    );
}

/// RFC 8620 §3.7: a call carrying both `ids` and `#ids` is refused (`invalidArguments`), so
/// setting a reference must take the plain argument's place.
#[test]
fn a_result_reference_replaces_the_plain_argument() {
    let of = CallId::from_wire("4");
    let mut arguments = Map::new();
    arguments.insert("ids".to_owned(), json!(["a"]));
    let reference = ResultRef::new(&of, "Email/query", "/ids");
    reference.set_on(&mut arguments, "ids");
    assert_eq!(arguments.get("ids"), None);
    assert_eq!(
        arguments.get("#ids"),
        Some(&json!({ "resultOf": "4", "name": "Email/query", "path": "/ids" }))
    );
    assert_eq!(reference.of(), &of);
    assert_eq!(reference.name(), "Email/query");
    assert_eq!(reference.path(), "/ids");
}

#[test]
fn a_call_id_shows_as_it_is_sent() {
    assert_eq!(CallId::from_wire("7").to_string(), "7");
}

#[test]
fn parses_responses_and_finds_by_call_id() {
    let mut req = Request::new([capability::CORE]);
    let first = req.invoke("Mailbox/get", json!({}));
    let second = req.invoke("Email/query", json!({}));
    let third = req.invoke("Thread/get", json!({}));
    let body = json!({
        "methodResponses": [
            ["Mailbox/get", { "accountId": "c", "list": [], "state": "s1" }, "0"],
            ["Email/query", { "ids": ["e1"] }, "1"]
        ],
        "sessionState": "abc"
    });
    let resp = Response::parse(&body).unwrap();
    assert_eq!(resp.result(&first).unwrap()["state"], "s1");
    assert_eq!(resp.result(&second).unwrap()["ids"], json!(["e1"]));
    match resp.result(&third) {
        Err(JmapError::MissingResponse(missing)) => assert_eq!(missing, third),
        other => panic!("expected a missing response, got {other:?}"),
    }
    assert_eq!(resp.into_result(&second).unwrap()["ids"], json!(["e1"]));
}

#[test]
fn method_error_response_becomes_typed_error() {
    let call = CallId::from_wire("0");
    let body = json!({
        "methodResponses": [
            ["error", { "type": "cannotCalculateChanges" }, "0"]
        ]
    });
    let resp = Response::parse(&body).unwrap();
    for err in [
        resp.result(&call).unwrap_err(),
        resp.clone().into_result(&call).unwrap_err(),
    ] {
        match err {
            JmapError::Method {
                call_id,
                error_type,
            } => {
                assert_eq!(call_id, call);
                assert_eq!(error_type, "cannotCalculateChanges");
            }
            other => panic!("expected method error, got {other:?}"),
        }
    }
}

#[test]
fn rejects_non_triple_responses() {
    for body in [
        json!({ "methodResponses": [["only", "two"]] }),
        json!({ "methodResponses": [[7, {}, "0"]] }),
        json!({ "methodResponses": [["Email/get", {}, 0]] }),
        json!({ "sessionState": "x" }),
    ] {
        assert!(Response::parse(&body).is_err(), "{body}");
    }
}
