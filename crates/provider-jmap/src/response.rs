//! The answer to a batched JMAP request (RFC 8620 §3.4): `{ "methodResponses": [[name, result,
//! callId], …], "sessionState": … }`, looked up by the [`CallId`] each call was given.

use serde_json::Value;

use crate::{
    error::JmapError,
    request::{CallId, Invocation},
};

/// The answer to a [`Request`](crate::Request), from
/// [`JmapClient::execute`](crate::JmapClient::execute).
///
/// Each call's answer is read by the [`CallId`] [`Request::invoke`](crate::Request::invoke)
/// returned for it. A call the server answered with a method error (RFC 8620 §3.6.2) reads as
/// [`JmapError::Method`], and one it did not answer at all as [`JmapError::MissingResponse`], so
/// a caller handles both where it reads, as a typed error.
///
/// # Examples
///
/// ```no_run
/// use provider_jmap::{JmapClient, JmapError, Request, capability};
/// use serde_json::json;
///
/// async fn inbox_unread(client: &JmapClient, inbox: &str) -> Result<u64, JmapError> {
///     let account = client.session().mail_account_id()?;
///     let mut request = Request::new([capability::CORE, capability::MAIL]);
///     let get = request.invoke(
///         "Mailbox/get",
///         json!({ "accountId": account, "ids": [inbox], "properties": ["unreadEmails"] }),
///     );
///     let response = client.execute(&request).await?;
///     Ok(response.result(&get)?["list"][0]["unreadEmails"]
///         .as_u64()
///         .unwrap_or(0))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Response {
    responses: Vec<Invocation>,
    session_state: Option<String>,
}

impl Response {
    /// Parses a `{ methodResponses, sessionState }` document.
    pub(crate) fn parse(value: &Value) -> Result<Self, JmapError> {
        let list = value
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| JmapError::protocol("methodResponses missing"))?;
        let responses = list
            .iter()
            .map(invocation_from_json)
            .collect::<Result<_, _>>()?;
        let session_state = value
            .get("sessionState")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            responses,
            session_state,
        })
    }

    /// The `sessionState` the answer carries (RFC 8620 §3.4): when it differs from the
    /// [`Session::state`](crate::Session::state) a client holds, the session changed and is
    /// fetched again. `None` where the server sent none.
    #[must_use]
    pub fn session_state(&self) -> Option<&str> {
        self.session_state.as_deref()
    }

    /// The answer to `call` named `name`, for a method that answers more than once under one
    /// call id: `EmailSubmission/set` with `onSuccessUpdateEmail` answers as itself, then as
    /// the implicit `Email/set` that filed the message (RFC 8621 §7.5).
    ///
    /// # Errors
    ///
    /// [`JmapError::Method`] when the server answered `call` with a method error, and
    /// [`JmapError::MissingResponse`] when no answer to `call` carries `name`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use provider_jmap::{CallId, JmapError, Response};
    ///
    /// fn filed(response: &Response, submission: &CallId) -> Result<bool, JmapError> {
    ///     let implicit = response.result_named(submission, "Email/set")?;
    ///     Ok(implicit
    ///         .get("updated")
    ///         .is_some_and(|updated| !updated.is_null()))
    /// }
    /// ```
    pub fn result_named(&self, call: &CallId, name: &str) -> Result<&Value, JmapError> {
        let mut answers = self.responses.iter().filter(|inv| &inv.call_id == call);
        if let Some(error) = answers.clone().find(|inv| inv.method == "error") {
            answered(error)?;
        }
        answers
            .find(|inv| inv.method == name)
            .map(|inv| &inv.arguments)
            .ok_or_else(|| JmapError::MissingResponse(call.clone()))
    }

    /// The result arguments the server answered `call` with: its first answer, where a method
    /// answers more than once under one id (RFC 8620 §3.4).
    ///
    /// # Errors
    ///
    /// [`JmapError::Method`] when the server answered `call` with a method error, and
    /// [`JmapError::MissingResponse`] when it did not answer it.
    pub fn result(&self, call: &CallId) -> Result<&Value, JmapError> {
        let invocation = self
            .responses
            .iter()
            .find(|inv| &inv.call_id == call)
            .ok_or_else(|| JmapError::MissingResponse(call.clone()))?;
        answered(invocation)?;
        Ok(&invocation.arguments)
    }

    /// [`result`](Self::result), taking the arguments out of the response rather than borrowing
    /// them.
    ///
    /// # Errors
    ///
    /// As [`result`](Self::result).
    pub fn into_result(mut self, call: &CallId) -> Result<Value, JmapError> {
        let at = self
            .responses
            .iter()
            .position(|inv| &inv.call_id == call)
            .ok_or_else(|| JmapError::MissingResponse(call.clone()))?;
        let invocation = self.responses.swap_remove(at);
        answered(&invocation)?;
        Ok(invocation.arguments)
    }
}

/// The method error (RFC 8620 §3.6.2) `invocation` is, if it is one.
fn answered(invocation: &Invocation) -> Result<(), JmapError> {
    if invocation.method != "error" {
        return Ok(());
    }
    let error_type = invocation
        .arguments
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    Err(JmapError::Method {
        call_id: invocation.call_id.clone(),
        error_type,
    })
}

/// Parses one `[name, args, callId]` triple from a response.
fn invocation_from_json(value: &Value) -> Result<Invocation, JmapError> {
    let triple = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| JmapError::protocol("method response is not a triple"))?;
    let method = triple[0]
        .as_str()
        .ok_or_else(|| JmapError::protocol("method name is not a string"))?;
    let call_id = triple[2]
        .as_str()
        .ok_or_else(|| JmapError::protocol("call id is not a string"))?;
    Ok(Invocation {
        method: method.to_owned(),
        arguments: triple[1].clone(),
        call_id: CallId::from_wire(call_id),
    })
}
