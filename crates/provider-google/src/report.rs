//! Reporting a message as junk or not junk via `messages.modify`.
//!
//! Gmail has no report endpoint. Its filter learns from the `SPAM` label, so on this
//! transport **the label is the report** — not a move that happens to accompany one.
//! Two behaviours were established against a live account, and both shape this module:
//!
//! - **Adding `SPAM` files the message by itself.** The server drops `INBOX` without being asked
//!   (`["UNREAD","SENT","INBOX"]` → `["UNREAD","SENT","SPAM"]`), so there is no separate move to
//!   make and no way to report without moving.
//! - **Removing `SPAM` does *not* put the message back.** It leaves it in no place label at all —
//!   archived, and gone from the folder the user was looking at. So the not-junk direction must add
//!   the destination explicitly; that is the whole reason [`MessageReport::destination`] is read
//!   here rather than ignored as it is on Graph.
//!
//! There is no phishing verdict. Gmail's system label set has no member for it and
//! `messages.modify` answers `400 Invalid label` for anything outside the set, so the
//! adapter advertises [`ReportVerdicts::without_phishing`] and a phishing report is
//! refused before it reaches the wire rather than being filed as junk.

use engine_core::ids::ProviderKey;
use engine_provider::{MessageReport, ProviderError, ProviderResult, ReportReceipt, ReportVerdict};

use crate::{error::GoogleError, transport::GoogleClient};

/// The Gmail system label that both *is* the junk report and files the message.
const SPAM_LABEL: &str = "SPAM";

/// Reports `report.target`, returning a receipt carrying the (unchanged) message key.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) from the underlying
/// `modify`: a stale target is a
/// [`Conflict`](engine_core::error::FailureClass::Conflict); auth / rate-limit /
/// retryable map from the HTTP status.
pub(crate) async fn report_message(
    client: &GoogleClient,
    report: &MessageReport,
) -> ProviderResult<ReportReceipt> {
    let (add, remove) = label_delta(report)?;
    let body = serde_json::json!({ "addLabelIds": add, "removeLabelIds": remove });
    let body = serde_json::to_vec(&body).map_err(GoogleError::from)?;
    client
        .post(
            &client.url(&format!(
                "/gmail/v1/users/me/messages/{}/modify",
                report.target.as_str()
            )),
            "application/json",
            body,
        )
        .await?;
    Ok(ReportReceipt::new(ProviderKey::clone(&report.target)))
}

/// The `addLabelIds`/`removeLabelIds` delta for one report.
///
/// Junk adds `SPAM` and nothing else — the server clears the place labels itself, and
/// `SPAM` rather than `destination` is what the filter reads, so a host that resolved
/// the Junk mailbox to something unexpected still trains Gmail rather than silently
/// filing the message somewhere quiet.
///
/// Not-junk drops `SPAM` **and** adds the destination, because dropping alone archives
/// the message.
/// Phishing is rejected **here**, not only by the capability check upstream. Mapping it
/// onto `SPAM` would be the silent downgrade the capability exists to prevent, and a
/// guard that lives in another module is one refactor away from being skipped.
fn label_delta(report: &MessageReport) -> ProviderResult<(Vec<&str>, Vec<&str>)> {
    Ok(match report.verdict {
        ReportVerdict::Junk => (vec![SPAM_LABEL], Vec::new()),
        ReportVerdict::NotJunk => (vec![report.destination.as_str()], vec![SPAM_LABEL]),
        ReportVerdict::Phishing => {
            return Err(ProviderError::invalid_state(
                "Gmail has no phishing verdict; read Capabilities::mail_report before \
                 offering it",
            ));
        }
    })
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
