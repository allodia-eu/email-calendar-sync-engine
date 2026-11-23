//! The out-of-office reply: JMAP's `VacationResponse` (RFC 8621 §8), as the neutral
//! [`Vacation`] (`filters-and-out-of-office.md`, slice 1).
//!
//! - **Reached only where the account has it.** [`reachable`] is the session's own answer for
//!   `urn:ietf:params:jmap:vacationresponse` ([`Session::has_capability`] and
//!   [`Session::account_for`]), so a client bound to a read-only share, which lists no such
//!   capability, has no reply.
//! - **A setting, written directly.** `VacationResponse/set` updates the singleton with every
//!   modelled property, guarded by `ifInState` with the version the caller read (RFC 8620 §5.3): a
//!   stale one is `stateMismatch`, a `Conflict`. A `VacationResponse` holds a window and a subject
//!   and answers everyone with one reply ([`vocabulary`]).
//! - **What the write did to the account's own rules is observed, not assumed.** Where the account
//!   also has JMAP Sieve, the same request lists the scripts before and after the update. Stalwart
//!   runs the reply as a script of its own, and turning it on deactivates whatever script the user
//!   had active (RFC 9661 §2.1 allows one), which the receipt reports as
//!   [`RulesEffect::Deactivated`]. Without Sieve the rules cannot be seen, so the effect is
//!   [`RulesEffect::Unknown`].
//! - **No content in errors.** A failure names its method error or `SetError` type; the reply's
//!   words never reach a message.

use std::collections::BTreeMap;

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawVacation,
    time::UtcDateTime,
    vacation::{
        AutoReply, RulesEffect, VacationReceipt, VacationResponse, VacationVocabulary,
        VacationWindow,
    },
    version::VacationVersion,
};
use engine_provider::{ProviderResult, Vacation, WriteGuard, refuse_unheld_vacation};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    error::{JmapError, set_refusal},
    executor::Executor,
    request::{Request, capability},
    session::Session,
    session_accounts::JmapAccountId,
};

/// The one `VacationResponse` an account has (RFC 8621 §8).
const SINGLETON: &str = "singleton";

/// Whether the session gives this client an account with a reply: the server speaks the
/// capability and addresses its calls at an account of this client's.
pub(crate) fn reachable(session: &Session) -> bool {
    session.has_capability(capability::VACATION_RESPONSE)
        && session.account_for(capability::VACATION_RESPONSE).is_some()
}

/// What a `VacationResponse` holds: a window and a subject, one reply, to every sender.
pub(crate) fn vocabulary() -> VacationVocabulary {
    VacationVocabulary::everyone().with_window().with_subject()
}

#[async_trait]
impl Vacation for JmapProvider {
    fn vocabulary(&self) -> VacationVocabulary {
        vocabulary()
    }

    /// `ifInState` on the `VacationResponse` type: the type has one object, so its state is the
    /// reply's own version, and a stale one is refused (proven live, `tests/live_vacation.rs`).
    fn guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        Ok(read(self.executor()).await?)
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        base: &VacationResponse,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        refuse_unheld_vacation(&vocabulary(), new)?;
        Ok(write(self.executor(), base, new).await?)
    }
}

/// The account's reply, and the state it was read at.
async fn read(executor: &dyn Executor) -> Result<VacationResponse, JmapError> {
    let account = vacation_account(executor.session())?;
    let mut request = Request::new([capability::CORE, capability::VACATION_RESPONSE]);
    let get = request.invoke(
        "VacationResponse/get",
        json!({ "accountId": account, "ids": [SINGLETON] }),
    );
    let response = executor.execute(&request).await?;
    let result = response.result(&get)?;
    let object = result
        .get("list")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|object| object.get("id").and_then(Value::as_str) == Some(SINGLETON))
        })
        .ok_or_else(|| JmapError::protocol("VacationResponse/get answered no singleton"))?;
    let mut reply = from_json(object)?;
    if let Some(state) = result.get("state").and_then(Value::as_str) {
        reply = reply.with_version(VacationVersion::new(state));
    }
    Ok(reply)
}

/// Replaces the reply with `new`, guarded by `base`'s version, and lists the account's Sieve
/// scripts around it where it can see them.
async fn write(
    executor: &dyn Executor,
    base: &VacationResponse,
    new: &VacationResponse,
) -> Result<VacationReceipt, JmapError> {
    let session = executor.session();
    let account = vacation_account(session)?;
    let sieve = session
        .has_capability(capability::SIEVE)
        .then(|| session.account_for(capability::SIEVE))
        .flatten();

    let mut using = vec![capability::CORE, capability::VACATION_RESPONSE];
    if sieve.is_some() {
        using.push(capability::SIEVE);
    }
    let mut request = Request::new(using);
    let scripts = |request: &mut Request, sieve: &JmapAccountId| {
        request.invoke(
            "SieveScript/get",
            json!({ "accountId": sieve, "ids": null, "properties": ["isActive"] }),
        )
    };
    let before = sieve.map(|sieve| scripts(&mut request, sieve));
    let mut arguments = json!({
        "accountId": account,
        "update": { SINGLETON: to_json(new) },
    });
    if let Some(version) = &base.version {
        arguments["ifInState"] = json!(version.as_str());
    }
    let set = request.invoke("VacationResponse/set", arguments);
    let after = sieve.map(|sieve| scripts(&mut request, sieve));

    let response = executor
        .execute(&request)
        .await
        .map_err(JmapError::after_write)?;
    let result = response.result(&set).map_err(JmapError::after_write)?;
    if let Some(refusal) = set_refusal(result, "notUpdated", SINGLETON) {
        return Err(JmapError::refused(SINGLETON, refusal));
    }
    if result
        .get("updated")
        .and_then(|u| u.get(SINGLETON))
        .is_none()
    {
        return Err(JmapError::unanswered(SINGLETON, "updated", "notUpdated"));
    }
    let version = result
        .get("newState")
        .and_then(Value::as_str)
        .map(VacationVersion::new);

    let rules = match (before, after) {
        (Some(before), Some(after)) => match (
            response.result(&before).ok().and_then(active_by_id),
            response.result(&after).ok().and_then(active_by_id),
        ) {
            (Some(before), Some(after)) => effect(&before, &after, new.enabled),
            _ => RulesEffect::Unknown,
        },
        _ => RulesEffect::Unknown,
    };
    Ok(VacationReceipt::new(version, rules))
}

/// The account `VacationResponse` calls address.
fn vacation_account(session: &Session) -> Result<&JmapAccountId, JmapError> {
    if !reachable(session) {
        return Err(JmapError::session(
            "this account has no out-of-office reply (no vacationresponse capability)",
        ));
    }
    session
        .account_for(capability::VACATION_RESPONSE)
        .ok_or_else(|| JmapError::session("no account for the out-of-office reply"))
}

/// Each script's id and whether it is active, from a `SieveScript/get` answer.
fn active_by_id(result: &Value) -> Option<BTreeMap<String, bool>> {
    result
        .get("list")?
        .as_array()?
        .iter()
        .map(|script| {
            let id = script.get("id")?.as_str()?.to_owned();
            Some((id, script.get("isActive")?.as_bool()?))
        })
        .collect()
}

/// What the write did to the account's own scripts: a script active before that is still there
/// and inactive after, while the reply is now on, was switched off for the reply. A reply
/// switched off takes only its own script with it, so it deactivates nothing of the user's.
fn effect(
    before: &BTreeMap<String, bool>,
    after: &BTreeMap<String, bool>,
    turned_on: bool,
) -> RulesEffect {
    let switched_off = before
        .iter()
        .filter(|(_, active)| **active)
        .any(|(id, _)| after.get(id) == Some(&false));
    if turned_on && switched_off {
        RulesEffect::Deactivated
    } else {
        RulesEffect::Untouched
    }
}

/// A `VacationResponse` object as the neutral reply, its JSON kept as the raw form.
fn from_json(object: &Value) -> Result<VacationResponse, JmapError> {
    let enabled = object
        .get("isEnabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| JmapError::protocol("a VacationResponse without isEnabled"))?;
    let mut window = VacationWindow::default();
    if let Some(start) = date(object, "fromDate")? {
        window = window.with_start(start);
    }
    if let Some(end) = date(object, "toDate")? {
        window = window.with_end(end);
    }
    let mut reply = AutoReply::default();
    if let Some(subject) = text(object, "subject") {
        reply = reply.with_subject(subject);
    }
    if let Some(body) = text(object, "textBody") {
        reply = reply.with_text(body);
    }
    if let Some(body) = text(object, "htmlBody") {
        reply = reply.with_html(body);
    }
    let read = VacationResponse::default()
        .with_window(window)
        .with_reply(reply)
        .with_raw(RawVacation::new(object.to_string()));
    Ok(if enabled {
        read.turned_on()
    } else {
        read.turned_off()
    })
}

/// The neutral reply as the properties of a `VacationResponse` update: every one, so a part
/// the reply leaves out is cleared rather than kept from before.
fn to_json(reply: &VacationResponse) -> Value {
    json!({
        "isEnabled": reply.enabled,
        "fromDate": reply.window.start.map(|at| at.to_string()),
        "toDate": reply.window.end.map(|at| at.to_string()),
        "subject": reply.reply.subject,
        "textBody": reply.reply.text,
        "htmlBody": reply.reply.html,
    })
}

/// A `UTCDate` property, `None` when null or absent. A malformed one names the property, never
/// its value.
fn date(object: &Value, property: &str) -> Result<Option<UtcDateTime>, JmapError> {
    match object.get(property) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(|text| UtcDateTime::parse_rfc3339(text).ok())
            .map(Some)
            .ok_or_else(|| {
                JmapError::protocol(format!(
                    "a VacationResponse {property} that is not a UTCDate"
                ))
            }),
    }
}

/// A `String|null` property.
fn text(object: &Value, property: &str) -> Option<String> {
    object.get(property)?.as_str().map(str::to_owned)
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
