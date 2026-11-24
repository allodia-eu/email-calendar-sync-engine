//! The out-of-office reply: JMAP's `VacationResponse` (RFC 8621 §8), as the neutral
//! [`Vacation`] (`filters-and-out-of-office.md`, slice 1).
//!
//! - **Reached only where the account has it.** [`reachable`] is the session's own answer for
//!   `urn:ietf:params:jmap:vacationresponse` ([`Session::extension_account`]), so a client bound to
//!   a read-only share, which lists no such capability, has no reply.
//! - **A setting, written directly.** `VacationResponse/set` updates the singleton with every
//!   modelled property. A guarded write sends `ifInState` with the version the caller read (RFC
//!   8620 §5.3), and a stale one is `stateMismatch`, a `Conflict`; only an
//!   [`Unconditional`](VacationPrecondition::Unconditional) write the caller spelled out sends
//!   none. A `VacationResponse` holds a window and a subject and answers everyone with one reply
//!   ([`vocabulary`]).
//! - **While the user's rules run, the reply runs through them** (`crate::vacation_include`): where
//!   a script of the user's runs and the condition holds (`jmap.md`, "the condition"), the words
//!   are written with `isEnabled: false`, which activates nothing, and the rules script's `include`
//!   of the reply's script switches it on and off, so the user's rules keep running
//!   ([`RulesEffect::Untouched`]). A read lists the scripts in the same request and reads the reply
//!   as on where that include runs it.
//! - **Otherwise the server's own flag, and what it did to the rules is observed, not assumed.**
//!   Where the account also has JMAP Sieve, the same request lists the scripts before and after the
//!   update, and an effect is claimed only where the two listings' states are the write's
//!   `oldState` and `newState`, so nothing else changed between them. Turning the reply on with the
//!   flag deactivates whatever script ran (RFC 9661 §2.1 allows one), reported as
//!   [`RulesEffect::Deactivated`]: on Stalwart only where the include path did not apply (the
//!   condition failed, or the running script could not take the include). Turning it off switches
//!   nothing back on, reported as [`RulesEffect::StillDeactivated`]. Without Sieve the rules cannot
//!   be seen, so the effect is [`RulesEffect::Unknown`].
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
        VacationWindow, WindowSupport,
    },
    version::VacationVersion,
};
use engine_provider::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard,
    refuse_unheld_vacation,
};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    error::{JmapError, set_refusal},
    executor::Executor,
    request::{Request, capability},
    rules::WhenItStops,
    session::Session,
    session_accounts::JmapAccountId,
};

/// The one `VacationResponse` an account has (RFC 8621 §8).
const SINGLETON: &str = "singleton";

/// Whether the session gives this client an account with a reply
/// ([`Session::extension_account`], the one lookup).
pub(crate) fn reachable(session: &Session) -> bool {
    session
        .extension_account(capability::VACATION_RESPONSE)
        .is_some()
}

/// What a `VacationResponse` holds: a window with either end (`fromDate`, `toDate`), a subject,
/// a text and an HTML body, one reply, to every sender. The same for every account, so asking
/// costs no request.
fn vocabulary() -> VacationVocabulary {
    VacationVocabulary::everyone()
        .with_window(WindowSupport::EitherEnd)
        .with_subject()
        .with_text_and_html()
}

#[async_trait]
impl Vacation for JmapProvider {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        Ok(vocabulary())
    }

    /// `ifInState` on the `VacationResponse` type: the type has one object, so its state is the
    /// reply's own version, and a stale one is refused (proven live, `tests/live_vacation.rs`).
    fn vacation_write_guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        Ok(read(self.executor()).await?)
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        set(self, precondition, new, WhenItStops::Report).await
    }
}

/// Replaces the reply with `new`, under `precondition`: [`Vacation::set_vacation`], and an import's
/// write, which `stops` makes refuse where the reply would switch the user's running rules off.
pub(crate) async fn set(
    provider: &JmapProvider,
    precondition: VacationPrecondition<'_>,
    new: &VacationResponse,
    stops: WhenItStops,
) -> ProviderResult<VacationReceipt> {
    refuse_unheld_vacation(&vocabulary(), new)?;
    let guard = precondition.version(provider.vacation_write_guard())?;
    let executor = provider.executor();
    // While the user's rules run, through their include where it can be; otherwise with the
    // server's own flag, as without rules (`crate::vacation_include::path`).
    match crate::vacation_include::path(executor, new.enabled).await? {
        crate::vacation_include::Path::Include { prepared, toggled } => {
            let account = vacation_account(executor.session())?;
            let body = to_json(new);
            crate::vacation_include::write(executor, account, guard, &prepared, toggled, body).await
        }
        crate::vacation_include::Path::Flag { rules_running } => {
            // On by the flag, the reply's script becomes the one that runs, and the user's stops.
            if new.enabled && rules_running && stops == WhenItStops::Refuse {
                return Err(ProviderError::item_unavailable(
                    "turning this out-of-office reply on would switch the account's running mail \
                     rules off, since the rules script cannot run it through its include",
                ));
            }
            Ok(write(executor, guard, new).await?)
        }
    }
}

/// The account's reply, and the state it was read at. Where the account has Sieve, the same
/// request lists the scripts, and a reply its own flag calls off is on where the user's rules
/// script runs it (`crate::vacation_include`, [`reply_runs`](crate::sieve_scripts::reply_runs)).
async fn read(executor: &dyn Executor) -> Result<VacationResponse, JmapError> {
    let session = executor.session();
    let account = vacation_account(session)?;
    let sieve = session.extension_account(capability::SIEVE);
    let mut using = vec![capability::CORE, capability::VACATION_RESPONSE];
    if sieve.is_some() {
        using.push(capability::SIEVE);
    }
    let mut request = Request::new(using);
    let get = request.invoke(
        "VacationResponse/get",
        json!({ "accountId": account, "ids": [SINGLETON] }),
    );
    let listing = sieve.map(|sieve| crate::sieve_scripts::list_call(&mut request, sieve));
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
    // The state is the version a write is guarded by; a reply read without one could only be
    // written unguarded, so it is refused here rather than there.
    let state = result
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::protocol("VacationResponse/get answered without a state"))?;
    let reply = from_json(object)?.with_version(VacationVersion::new(state));
    // A listing that failed says nothing about the rules: the reply reads as its flag says.
    let scripts = listing
        .filter(|_| !reply.enabled)
        .and_then(|call| response.result(&call).ok())
        .and_then(|result| crate::sieve_scripts::parse(result).ok());
    // The include runs the reply at delivery whatever the counters are: on, as the server does.
    if let Some(scripts) = scripts
        && crate::vacation_include::runs(executor, &scripts).await
    {
        return Ok(reply.turned_on());
    }
    Ok(reply)
}

/// Replaces the reply with `new`, guarded by `guard` where there is one, and lists the account's
/// Sieve scripts around it where it can see them.
async fn write(
    executor: &dyn Executor,
    guard: Option<&VacationVersion>,
    new: &VacationResponse,
) -> Result<VacationReceipt, JmapError> {
    let session = executor.session();
    let account = vacation_account(session)?;
    let sieve = session.extension_account(capability::SIEVE);

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
    if let Some(version) = guard {
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
    let state = |key| result.get(key).and_then(Value::as_str);
    let version = state("newState").map(VacationVersion::new);

    let rules = match (before, after) {
        (Some(before), Some(after)) => {
            match (listing(&response, &before), listing(&response, &after)) {
                // One counter for both types (Stalwart): the listings bracket the write exactly, so
                // nothing came between them, and what changed is the write's doing.
                (Some((before_state, before)), Some((after_state, after)))
                    if Some(before_state) == state("oldState")
                        && Some(after_state) == state("newState") =>
                {
                    effect(&before, &after, new.enabled)
                }
                _ => RulesEffect::Unknown,
            }
        }
        _ => RulesEffect::Unknown,
    };
    Ok(VacationReceipt::new(version, rules))
}

/// The account `VacationResponse` calls address.
fn vacation_account(session: &Session) -> Result<&JmapAccountId, JmapError> {
    session
        .extension_account(capability::VACATION_RESPONSE)
        .ok_or_else(|| {
            JmapError::session(
                "this account has no out-of-office reply (no vacationresponse capability)",
            )
        })
}

/// A `SieveScript/get` answer's state and scripts, or `None` where it failed or cannot be read.
fn listing<'a>(
    response: &'a crate::Response,
    call: &crate::CallId,
) -> Option<(&'a str, BTreeMap<String, bool>)> {
    let result = response.result(call).ok()?;
    Some((result.get("state")?.as_str()?, active_by_id(result)?))
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

/// What the write did to the account's own scripts, from the scripts that ran before it and are
/// still there, inactive, after it.
///
/// - The reply went on: such a script was switched off for it ([`RulesEffect::Deactivated`]).
/// - The reply went off: such a script is the reply's own. If nothing runs now while the account
///   has other scripts, the user's rules stay off ([`RulesEffect::StillDeactivated`]).
/// - No script went off: nothing of the user's was touched.
fn effect(
    before: &BTreeMap<String, bool>,
    after: &BTreeMap<String, bool>,
    turned_on: bool,
) -> RulesEffect {
    let went_off: Vec<&String> = before
        .iter()
        .filter(|(id, active)| **active && after.get(*id) == Some(&false))
        .map(|(id, _)| id)
        .collect();
    let nothing_runs = !after.values().any(|active| *active);
    let others = after.keys().any(|id| !went_off.contains(&id));
    if went_off.is_empty() {
        RulesEffect::Untouched
    } else if turned_on {
        RulesEffect::Deactivated
    } else if nothing_runs && others {
        RulesEffect::StillDeactivated
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
    if let Some(subject) = text(object, "subject")? {
        reply = reply.with_subject(subject);
    }
    if let Some(body) = text(object, "textBody")? {
        reply = reply.with_text(body);
    }
    if let Some(body) = text(object, "htmlBody")? {
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

/// A `String|null` property, `None` when null or absent. Anything else is refused, as a date
/// is, naming the property and never its value: one leniency for every property.
fn text(object: &Value, property: &str) -> Result<Option<String>, JmapError> {
    match object.get(property) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(JmapError::protocol(format!(
            "a VacationResponse {property} that is not a string"
        ))),
    }
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "vacation_read_tests.rs"]
mod read_tests;
