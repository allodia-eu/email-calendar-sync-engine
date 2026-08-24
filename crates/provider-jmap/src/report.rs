//! Reporting a message as junk / not junk / phishing via `Email/set` (RFC 8621 §4.6).
//!
//! JMAP has no report *action*. The report is the IANA-registered keyword — `$junk`,
//! `$notjunk`, `$phishing` (RFC 8621 §4.1.1, RFC 9979) — and RFC 8621 says clients
//! SHOULD set it "to help train automated spam-detection systems". That is a
//! client-side SHOULD with no server obligation, no capability to probe and no error
//! if the server ignores it, so the adapter advertises
//! [`ReportEvidence::Convention`]: verified against Stalwart, the keyword is stored
//! and read back, and nothing in the protocol says what was done with it.
//!
//! Both halves ride **one** `Email/set` — the keyword patch and the `mailboxIds`
//! replacement that files the message — so a report and its move cannot land
//! half-applied and cost one round-trip. Live-verified against the harness.
//!
//! The contradicting keyword is cleared in the same patch. Leaving `$notjunk` set
//! while setting `$junk` would store a message that is asserted to be both, and which
//! of the two a server's classifier then believes is not something a client should be
//! deciding by accident.

use async_trait::async_trait;
use engine_core::ids::AccountId;
use engine_provider::{
    MessageReport, Provider, ProviderResult, ReportReceipt, ReportVerdict, ReportingProvider,
};
use serde_json::{Map, Value, json};

use crate::{
    JmapProvider,
    error::JmapError,
    executor::Executor,
    mutate::{check_set_result_for, keyword_pointer, update_args},
    request::{Request, capability},
};

/// Applies `report` to its target under `mail_account` via `Email/set`, returning a
/// receipt carrying the (unchanged) target key.
///
/// # Errors
///
/// Returns [`JmapError`] on a transport/method failure, or [`JmapError::Set`] when the
/// server rejects the object with a `SetError` — a `notFound` is a
/// [`Conflict`](engine_core::error::FailureClass::Conflict), matching the edit path.
pub(crate) async fn report_message(
    executor: &dyn Executor,
    mail_account: &str,
    report: &MessageReport,
) -> Result<ReportReceipt, JmapError> {
    let target = report.target.as_str();
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let call = req.invoke(
        "Email/set",
        update_args(mail_account, target, patch(report)),
    );
    let resp = executor.execute(&req).await?;
    check_set_result_for(resp.result(&call)?, target, "updated", "notUpdated")?;
    Ok(ReportReceipt::new(report.target.clone()))
}

/// The keyword a verdict sets, and the ones it contradicts.
///
/// "Not junk" clears **both** accusations, not just `$junk`: the user is saying the
/// message is legitimate, and leaving `$phishing` behind would keep the stronger of the
/// two claims standing against a message they just vouched for.
const fn keywords_for(verdict: ReportVerdict) -> (&'static str, &'static [&'static str]) {
    match verdict {
        ReportVerdict::Junk => ("$junk", &["$notjunk"]),
        ReportVerdict::NotJunk => ("$notjunk", &["$junk", "$phishing"]),
        ReportVerdict::Phishing => ("$phishing", &["$notjunk"]),
    }
}

/// The PatchObject for one report: the verdict's keyword set, the keywords that would
/// contradict it cleared, and the membership replaced with the destination.
fn patch(report: &MessageReport) -> Value {
    let (set, clear) = keywords_for(report.verdict);
    let mut patch = Map::new();
    patch.insert(keyword_pointer(set), Value::Bool(true));
    for keyword in clear {
        patch.insert(keyword_pointer(keyword), Value::Null);
    }
    patch.insert(
        "mailboxIds".to_owned(),
        json!({ report.destination.as_str(): true }),
    );
    Value::Object(patch)
}

#[async_trait]
impl ReportingProvider for JmapProvider {
    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        // Refuse a verdict this session did not advertise rather than filing it as
        // something else — the shared rule, so the adapter cannot drop what it claims.
        if let Some(controls) = self.connection_info().capabilities.mail_report() {
            controls.accept(report)?;
        }
        let account = self.mail_account()?;
        Ok(report_message(self.executor.as_ref(), &account, report).await?)
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
