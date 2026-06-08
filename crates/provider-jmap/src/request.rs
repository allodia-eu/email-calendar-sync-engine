//! The batched JMAP request/response envelope (RFC 8620 §3.2–3.7).
//!
//! A JMAP request is `{ "using": [capabilities], "methodCalls": [[name, args,
//! callId], …] }`; the response is `{ "methodResponses": [[name, result, callId],
//! …], "sessionState": … }`. A method's argument may be a **result reference**
//! (`{ "resultOf": callId, "name": method, "path": pointer }`) carried under a
//! `#`-prefixed key, letting one call consume a prior call's output without a
//! round-trip (RFC 8620 §3.7) — e.g. `Email/get` over the ids `Email/query`
//! produced.
//!
//! These types are pure (`serde_json` only) so the envelope is fully unit-tested
//! offline; the transport that ships them lives in [`crate::transport`].

use serde_json::{Map, Value, json};

/// JMAP capability URNs used in the `using` set.
pub(crate) mod capability {
    /// `urn:ietf:params:jmap:core` (RFC 8620). Required by every request.
    pub(crate) const CORE: &str = "urn:ietf:params:jmap:core";
    /// `urn:ietf:params:jmap:mail` (RFC 8621).
    pub(crate) const MAIL: &str = "urn:ietf:params:jmap:mail";
    /// `urn:ietf:params:jmap:submission` (RFC 8621 §7).
    pub(crate) const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
    /// `urn:ietf:params:jmap:calendars` (JMAP Calendars draft).
    pub(crate) const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
}

/// One method call: `[name, arguments, callId]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    /// The method name, e.g. `Email/get`.
    pub(crate) method: String,
    /// The method arguments object.
    pub(crate) arguments: Value,
    /// The client-assigned call id, unique within the request.
    pub(crate) call_id: String,
}

impl Invocation {
    fn to_json(&self) -> Value {
        json!([self.method, self.arguments, self.call_id])
    }

    /// Parses one `[name, args, callId]` triple from a response.
    fn from_json(value: &Value) -> Result<Self, super::error::JmapError> {
        let triple = value
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| super::error::JmapError::protocol("method response is not a triple"))?;
        let method = triple[0]
            .as_str()
            .ok_or_else(|| super::error::JmapError::protocol("method name is not a string"))?;
        let call_id = triple[2]
            .as_str()
            .ok_or_else(|| super::error::JmapError::protocol("call id is not a string"))?;
        Ok(Self {
            method: method.to_owned(),
            arguments: triple[1].clone(),
            call_id: call_id.to_owned(),
        })
    }
}

/// A batched JMAP request under construction.
#[derive(Debug, Clone, Default)]
pub(crate) struct Request {
    using: Vec<String>,
    calls: Vec<Invocation>,
}

impl Request {
    /// Starts a request declaring the capability URNs its methods rely on.
    pub(crate) fn new(using: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            using: using.into_iter().map(str::to_owned).collect(),
            calls: Vec::new(),
        }
    }

    /// Appends a method call and returns its assigned call id, so a later call can
    /// back-reference it.
    pub(crate) fn invoke(&mut self, method: impl Into<String>, arguments: Value) -> String {
        let call_id = self.calls.len().to_string();
        self.calls.push(Invocation {
            method: method.into(),
            arguments,
            call_id: call_id.clone(),
        });
        call_id
    }

    /// Serializes to the `{ using, methodCalls }` wire object.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "using": self.using,
            "methodCalls": self.calls.iter().map(Invocation::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Builds a result-reference value (`{ resultOf, name, path }`, RFC 8620 §3.7)
/// for use as a `#`-prefixed argument, e.g. `args["#ids"] = result_ref("0",
/// "Email/query", "/ids")`.
pub(crate) fn result_ref(result_of: &str, name: &str, path: &str) -> Value {
    json!({ "resultOf": result_of, "name": name, "path": path })
}

/// A parsed batched response.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    responses: Vec<Invocation>,
}

impl Response {
    /// Parses a `{ methodResponses, sessionState }` document.
    pub(crate) fn parse(value: &Value) -> Result<Self, super::error::JmapError> {
        let list = value
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| super::error::JmapError::protocol("methodResponses missing"))?;
        let responses = list
            .iter()
            .map(Invocation::from_json)
            .collect::<Result<_, _>>()?;
        Ok(Self { responses })
    }

    /// Returns the result arguments for `call_id`, mapping a method `error`
    /// response (RFC 8620 §3.6.2) to a typed [`JmapError::Method`](super::error::JmapError::Method).
    pub(crate) fn result(&self, call_id: &str) -> Result<&Value, super::error::JmapError> {
        let invocation = self
            .responses
            .iter()
            .find(|inv| inv.call_id == call_id)
            .ok_or_else(|| super::error::JmapError::MissingResponse(call_id.to_owned()))?;
        if invocation.method == "error" {
            let error_type = invocation
                .arguments
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            return Err(super::error::JmapError::Method {
                call_id: call_id.to_owned(),
                error_type,
            });
        }
        Ok(&invocation.arguments)
    }
}

/// Inserts a `#`-prefixed back-reference argument into an arguments object.
pub(crate) fn with_back_reference(
    mut arguments: Map<String, Value>,
    name: &str,
    reference: Value,
) -> Value {
    arguments.insert(format!("#{name}"), reference);
    Value::Object(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_batched_request_with_sequential_call_ids() {
        let mut req = Request::new([capability::CORE, capability::MAIL]);
        let q = req.invoke("Email/query", json!({ "accountId": "c", "limit": 50 }));
        assert_eq!(q, "0");
        let get_args = with_back_reference(
            json!({ "accountId": "c" }).as_object().unwrap().clone(),
            "ids",
            result_ref(&q, "Email/query", "/ids"),
        );
        let g = req.invoke("Email/get", get_args);
        assert_eq!(g, "1");

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
    fn parses_responses_and_finds_by_call_id() {
        let body = json!({
            "methodResponses": [
                ["Mailbox/get", { "accountId": "c", "list": [], "state": "s1" }, "0"],
                ["Email/query", { "ids": ["e1"] }, "1"]
            ],
            "sessionState": "abc"
        });
        let resp = Response::parse(&body).unwrap();
        assert_eq!(resp.result("0").unwrap()["state"], "s1");
        assert_eq!(resp.result("1").unwrap()["ids"], json!(["e1"]));
        assert!(matches!(
            resp.result("2"),
            Err(super::super::error::JmapError::MissingResponse(_))
        ));
    }

    #[test]
    fn method_error_response_becomes_typed_error() {
        let body = json!({
            "methodResponses": [
                ["error", { "type": "cannotCalculateChanges" }, "0"]
            ]
        });
        let resp = Response::parse(&body).unwrap();
        let err = resp.result("0").unwrap_err();
        match err {
            super::super::error::JmapError::Method {
                call_id,
                error_type,
            } => {
                assert_eq!(call_id, "0");
                assert_eq!(error_type, "cannotCalculateChanges");
            }
            other => panic!("expected method error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_triple_responses() {
        let body = json!({ "methodResponses": [["only", "two"]] });
        assert!(Response::parse(&body).is_err());
        let missing = json!({ "sessionState": "x" });
        assert!(Response::parse(&missing).is_err());
    }
}
