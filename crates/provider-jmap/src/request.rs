//! The batched JMAP request envelope (RFC 8620 §3.3) and the result references that chain
//! one call to another (§3.7).
//!
//! A JMAP request is `{ "using": [capabilities], "methodCalls": [[name, args, callId], …] }`.
//! A method's argument may be a **result reference** (`{ "resultOf": callId, "name": method,
//! "path": pointer }`) carried under a `#`-prefixed key, letting one call consume a prior call's
//! output without a round trip, such as `Email/get` over the ids `Email/query` produced.
//!
//! These types are pure (`serde_json` only), so the envelope is unit-tested offline; the
//! transport that ships them is [`JmapClient::execute`](crate::JmapClient::execute), and the
//! answer is a [`Response`](crate::Response).

use core::fmt;

use serde_json::{Map, Value, json};

/// The JMAP capability URNs a [`Request`] declares in its `using` set.
///
/// A request lists every capability its methods rely on, and the core one always (RFC 8620
/// §3.3). These are the ones the adapter itself speaks; a server's extension is named by its own
/// URN, which [`Request::new`] takes as readily.
pub mod capability {
    /// `urn:ietf:params:jmap:core` (RFC 8620). Required by every request.
    pub const CORE: &str = "urn:ietf:params:jmap:core";
    /// `urn:ietf:params:jmap:mail` (RFC 8621).
    pub const MAIL: &str = "urn:ietf:params:jmap:mail";
    /// `urn:ietf:params:jmap:submission` (RFC 8621 §7).
    pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
    /// `urn:ietf:params:jmap:calendars` (JMAP Calendars).
    pub const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
    /// `urn:ietf:params:jmap:contacts` (RFC 9610).
    pub const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
    /// `urn:ietf:params:jmap:blob` (RFC 9404): `Blob/get` and `Blob/upload`, many blobs in one
    /// method call.
    pub const BLOB: &str = "urn:ietf:params:jmap:blob";
}

/// The id of one method call within a [`Request`] (RFC 8620 §3.2, the `callId`).
///
/// [`Request::invoke`] hands one out for each call it adds. It is the key a
/// [`Response`](crate::Response) answers that call under and the call a [`ResultRef`] points at.
/// It is a type of its own so that it cannot be mixed up with a method name or an object id,
/// which are strings on the wire too.
///
/// # Examples
///
/// ```
/// use provider_jmap::{Request, capability};
/// use serde_json::json;
///
/// let mut request = Request::new([capability::CORE]);
/// let echo = request.invoke("Core/echo", json!({ "hello": true }));
/// assert_eq!(request.to_json()["methodCalls"][0][2], echo.as_str());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallId(String);

impl CallId {
    /// A call id as the server answered it.
    pub(crate) fn from_wire(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as it is sent.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A result reference (RFC 8620 §3.7): "the value at `path` in the answer to call `of`, which
/// answered as `name`".
///
/// Set it on a later call's arguments with [`set_on`](Self::set_on), which writes it under the
/// `#`-prefixed key the protocol requires. The server resolves it before running that call, so
/// two dependent calls take one round trip.
///
/// # Examples
///
/// ```
/// use provider_jmap::{Request, ResultRef, capability};
/// use serde_json::{Map, json};
///
/// let mut request = Request::new([capability::CORE, capability::MAIL]);
/// let query = request.invoke("Email/query", json!({ "accountId": "a1", "limit": 10 }));
///
/// let mut get = Map::new();
/// get.insert("accountId".to_owned(), json!("a1"));
/// ResultRef::new(&query, "Email/query", "/ids").set_on(&mut get, "ids");
/// request.invoke("Email/get", get.into());
///
/// assert_eq!(
///     request.to_json()["methodCalls"][1][1]["#ids"],
///     json!({ "resultOf": query.as_str(), "name": "Email/query", "path": "/ids" })
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRef {
    of: CallId,
    name: String,
    path: String,
}

impl ResultRef {
    /// A reference to the value at `path` (a JSON pointer, where `*` maps over an array) in the
    /// answer to `of`, an answer named `name`: the method `of` called, or the implicit call a
    /// method answers with (`Email/set` after an `EmailSubmission/set`, RFC 8621 §7.5).
    #[must_use]
    pub fn new(of: &CallId, name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            of: of.clone(),
            name: name.into(),
            path: path.into(),
        }
    }

    /// The call whose answer is referenced.
    #[must_use]
    pub fn of(&self) -> &CallId {
        &self.of
    }

    /// The name the referenced answer carries.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The JSON pointer into the referenced answer.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Sets this reference as the argument `property` of `arguments`, under the `#property` key
    /// the protocol requires. A plain `property` there is removed, since RFC 8620 §3.7 refuses a
    /// call that carries both.
    pub fn set_on(&self, arguments: &mut Map<String, Value>, property: &str) {
        arguments.remove(property);
        arguments.insert(
            format!("#{property}"),
            json!({ "resultOf": self.of.as_str(), "name": self.name, "path": self.path }),
        );
    }
}

/// One method call: `[name, arguments, callId]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    /// The method name, such as `Email/get`.
    pub(crate) method: String,
    /// The method arguments object.
    pub(crate) arguments: Value,
    /// The call id, unique within the request.
    pub(crate) call_id: CallId,
}

impl Invocation {
    fn to_json(&self) -> Value {
        json!([self.method, self.arguments, self.call_id.as_str()])
    }
}

/// A batched JMAP request under construction (RFC 8620 §3.3).
///
/// Each [`invoke`](Self::invoke) appends a method call and returns its [`CallId`], which later
/// calls reference ([`ResultRef`]) and the [`Response`](crate::Response) answers it under. The
/// server runs the calls in order, and refuses a request with more calls than the session's
/// `maxCallsInRequest` ([`CoreLimits`](crate::CoreLimits)).
///
/// # Examples
///
/// ```no_run
/// use provider_jmap::{JmapClient, JmapError, Request, capability};
/// use serde_json::json;
///
/// async fn mailbox_names(client: &JmapClient) -> Result<Vec<String>, JmapError> {
///     let account = client.session().mail_account_id()?;
///     let mut request = Request::new([capability::CORE, capability::MAIL]);
///     let get = request.invoke(
///         "Mailbox/get",
///         json!({ "accountId": account, "properties": ["name"] }),
///     );
///     let response = client.execute(&request).await?;
///     let names = response.result(&get)?["list"]
///         .as_array()
///         .into_iter()
///         .flatten()
///         .filter_map(|mailbox| mailbox["name"].as_str().map(str::to_owned))
///         .collect();
///     Ok(names)
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Request {
    using: Vec<String>,
    calls: Vec<Invocation>,
}

impl Request {
    /// Starts a request declaring the capability URNs its methods rely on: [`capability`]
    /// constants, or a server extension's own URN.
    #[must_use]
    pub fn new<I>(using: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        Self {
            using: using.into_iter().map(Into::into).collect(),
            calls: Vec::new(),
        }
    }

    /// Appends a call of `method` with `arguments`, and returns its id: a later call references
    /// its answer by it, and the [`Response`](crate::Response) answers it under it.
    pub fn invoke(&mut self, method: impl Into<String>, arguments: Value) -> CallId {
        let call_id = CallId(self.calls.len().to_string());
        self.calls.push(Invocation {
            method: method.into(),
            arguments,
            call_id: call_id.clone(),
        });
        call_id
    }

    /// The request as the `{ using, methodCalls }` object that goes on the wire.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "using": self.using,
            "methodCalls": self.calls.iter().map(Invocation::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Sets `reference` as the argument `name` of `arguments` ([`ResultRef::set_on`]) and returns
/// the arguments as a value, for a call built from a map.
pub(crate) fn with_back_reference(
    mut arguments: Map<String, Value>,
    name: &str,
    reference: &ResultRef,
) -> Value {
    reference.set_on(&mut arguments, name);
    Value::Object(arguments)
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
