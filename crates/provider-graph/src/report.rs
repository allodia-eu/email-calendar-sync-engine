//! Reporting a message as junk / not junk / phishing via `reportMessage`.
//!
//! `POST {beta}/messages/{id}/reportMessage` with a `ReportAction` and
//! `IsMessageMoveRequested`. Graph is the one transport here that takes a report as an
//! *action* rather than a flag, and answers whether it was accepted — which is why it
//! is the only adapter advertising [`ReportEvidence::Acknowledged`].
//!
//! Four things about this endpoint were established by driving it against a real
//! account, and three of them contradict the published documentation. They are stated
//! here because each one silently breaks an adapter written from the docs alone:
//!
//! - **It is beta-only.** There is no v1.0 `reportMessage`. Its v1.0 predecessors
//!   `markAsJunk`/`markAsNotJunk` are deprecated and stopped returning data on 2025-12-30, so beta
//!   is not a preview shortcut here — it is the only endpoint that works. Hence
//!   [`GraphClient::beta_url`], used by nothing else.
//! - **The response is not a `message`.** The docs say the action returns a message object; it
//!   returns a `reportMessageCommandResult` — `{"properties":[{"key":
//!   "Status","value":"Success"}]}`. [`check_reported`] reads that, because a 200 whose status is
//!   *not* `Success` would otherwise be a silent success.
//! - **`IsMessageMoveRequested: false` does not keep the message in place.** It moves to Junk
//!   regardless. Both the JSON boolean and the string `"false"` (the form the doc's own example
//!   uses) were tried, so this is the flag being ignored rather than the wrong type being sent. The
//!   adapter therefore sends `true` — the message is going to move, and claiming otherwise upstream
//!   would be a lie about what happened.
//! - **Only three of the five documented `reportAction` values exist.** `unknown` and
//!   `unknownFutureValue` are both `400 RequestBodyRead`.
//!
//! [`MessageReport::destination`] is not sent: Graph files the message itself and
//! offers nowhere to say where. The outcome is the same mailbox the caller would have
//! chosen — Junk for junk/phishing, the Inbox for not-junk — and no transport lets a
//! caller pick a *different* one, so nothing is lost by not sending it.
//!
//! The immutable id survives the move (verified live), so the receipt carries the
//! unchanged target — but only because every request sends
//! `Prefer: IdType="ImmutableId"`. Without that header the id is folder-scoped and 404s
//! the moment the message lands in Junk.

use engine_core::ids::ProviderKey;
use engine_provider::{MessageReport, ProviderError, ProviderResult, ReportReceipt, ReportVerdict};
use serde_json::{Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// The `reportAction` value for a verdict. Only these three are accepted.
const fn report_action(verdict: ReportVerdict) -> &'static str {
    match verdict {
        ReportVerdict::Junk => "junk",
        ReportVerdict::NotJunk => "notJunk",
        ReportVerdict::Phishing => "phish",
    }
}

/// Reports `report.target` to Microsoft, returning a receipt carrying the (immutable,
/// so unchanged) target key.
///
/// # Errors
///
/// A classified [`ProviderError`]: the underlying status classification for a non-2xx
/// (a `400` for a rejected action is `Permanent`, `429` a rate limit, `5xx`
/// retryable), or [`ProviderError::invalid_state`] if the action returned `200` with a
/// status other than `Success`.
pub(crate) async fn report_message(
    client: &GraphClient,
    report: &MessageReport,
) -> ProviderResult<ReportReceipt> {
    let path = format!("/messages/{}/reportMessage", report.target.as_str());
    let body = json!({
        "ReportAction": report_action(report.verdict),
        // The server moves the message whatever this says; `true` is what actually
        // happens, and the neutral contract promises the message is filed.
        "IsMessageMoveRequested": true,
    });
    let response = client
        .post(
            &client.beta_url(&path),
            "application/json",
            serde_json::to_vec(&body).map_err(GraphError::from)?,
        )
        .await?;
    check_reported(response.as_ref())?;
    Ok(ReportReceipt::new(ProviderKey::clone(&report.target)))
}

/// Reads the `reportMessageCommandResult` body, failing anything that is not
/// `Status: Success`.
///
/// A missing body or a missing `Status` is **accepted**: the action answered 2xx, and
/// the property bag is undocumented enough that treating its absence as a failure would
/// invent errors on a call the server took. What is not accepted is a `Status` that is
/// present and says something else — that is the one shape a 200 can use to mean "no".
fn check_reported(response: Option<&Value>) -> ProviderResult<()> {
    let Some(status) = response.and_then(report_status) else {
        return Ok(());
    };
    if status.eq_ignore_ascii_case("success") {
        return Ok(());
    }
    Err(ProviderError::invalid_state(format!(
        "Microsoft accepted the request but reported the message was not filed: {status}"
    )))
}

/// The `Status` value out of `{"properties":[{"key":"Status","value":"…"}]}`.
fn report_status(response: &Value) -> Option<&str> {
    response
        .get("properties")?
        .as_array()?
        .iter()
        .find(|entry| {
            entry
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| key.eq_ignore_ascii_case("status"))
        })?
        .get("value")?
        .as_str()
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
