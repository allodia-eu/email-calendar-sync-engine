//! The reply while the user's rules run: kept in the server's reply script, run by the rules
//! script's `include` (`crate::sieve_scripts`).
//!
//! On Stalwart the reply is a script of its own, and turning it on activates that script, which
//! switches the user's rules off (RFC 9661 §2.1). So while a script of the user's runs, the
//! reply is written as `VacationResponse/set` with `isEnabled: false`, which stores its words in
//! the reply's script and switches nothing, and then switched on or off by adding or removing
//! the rules script's `include` of it. A delivery test showed the included reply fires with
//! `isEnabled` false, which is why the include, not the flag, is what turns it off here.
//!
//! Two requests, because Stalwart does not resolve a result reference for `ifInState`: it
//! ignored `#ifInState` pointing at the first call's `newState` even when that call failed
//! (`docs/spec-deviations.md`). The second is guarded by the `newState` the first answered, so
//! nothing that landed between them is overwritten. If the first lands and the second does not,
//! the reply's words are stored and whether it runs is as it was: the write answers
//! [`requires_confirmation`](ProviderError::requires_confirmation), and the caller reads the
//! reply again before writing again.

use engine_core::{
    vacation::{RulesEffect, VacationReceipt, VacationResponse},
    version::VacationVersion,
};
use engine_provider::{ProviderError, ProviderResult};
use serde_json::{Value, json};

use crate::{
    error::{JmapError, set_refusal},
    executor::Executor,
    request::{Request, capability},
    rules::{self, sieve_account},
    sieve_scripts::{self, Script, Scripts, reply_runs},
};

/// The scripts and the user's running one, where the account has Sieve and a script of the
/// user's runs: the case this module writes the reply for. `None` otherwise, and the reply is
/// written with the server's own flag.
pub(crate) async fn rules_running(
    executor: &dyn Executor,
) -> Result<Option<(Scripts, Script)>, JmapError> {
    if !rules::reachable(executor.session()) {
        return Ok(None);
    }
    let scripts = rules::list(executor).await?;
    let running = scripts.rules_running().cloned();
    Ok(running.map(|running| (scripts, running)))
}

/// Whether the reply runs, as `scripts` and the running rules script's text say.
pub(crate) async fn runs(executor: &dyn Executor, scripts: &Scripts) -> Result<bool, JmapError> {
    let text = match scripts.rules_running() {
        Some(_) => Some(rules::rules_text(executor, scripts).await?),
        None => None,
    };
    Ok(reply_runs(scripts, text.as_deref()))
}

/// Writes `new` while the user's rules run ([module docs](self)).
pub(crate) async fn write(
    executor: &dyn Executor,
    vacation_account: &crate::session_accounts::JmapAccountId,
    guard: Option<&VacationVersion>,
    (scripts, running): &(Scripts, Script),
    new: &VacationResponse,
    body: Value,
) -> ProviderResult<VacationReceipt> {
    if guard.is_some_and(|version| version.as_str() != scripts.state) {
        return Err(ProviderError::conflict(
            "the out-of-office reply changed since it was read; read it again",
        ));
    }
    let account = sieve_account(executor.session())?;
    let text = sieve_scripts::download(executor, account, &running.blob).await?;
    let user = sieve_scripts::user_rules(&text);
    let toggled = sieve_scripts::compose(&text, &user, new.enabled).map_err(|err| {
        ProviderError::item_unavailable(format!(
            "the rules script cannot be edited to run the reply: {err}"
        ))
    })?;
    let blob = if toggled == text {
        None
    } else {
        Some(sieve_scripts::upload(executor, account, &toggled).await?)
    };

    // 1. The reply's words, switched off, so the server activates no script of its own.
    let mut update = body;
    update["isEnabled"] = json!(false);
    let mut arguments = json!({ "accountId": vacation_account, "update": { "singleton": update } });
    if let Some(version) = guard {
        arguments["ifInState"] = json!(version.as_str());
    }
    let mut request = Request::new([capability::CORE, capability::VACATION_RESPONSE]);
    let set = request.invoke("VacationResponse/set", arguments);
    let response = executor
        .execute(&request)
        .await
        .map_err(JmapError::after_write)?;
    let result = response.result(&set).map_err(JmapError::after_write)?;
    if let Some(refusal) = set_refusal(result, "notUpdated", "singleton") {
        return Err(JmapError::refused("singleton", refusal).into());
    }
    let written = result
        .get("newState")
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::unanswered("singleton", "updated", "notUpdated"))?
        .to_owned();
    let Some(blob) = blob else {
        return Ok(VacationReceipt::new(
            Some(VacationVersion::new(written)),
            RulesEffect::Untouched,
        ));
    };

    // 2. On or off: the include, guarded by the state the first write left.
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let set = request.invoke(
        "SieveScript/set",
        json!({
            "accountId": account,
            "ifInState": written,
            "update": { running.id.as_str(): { "blobId": blob.as_str() } },
        }),
    );
    let half_done = || {
        ProviderError::needs_confirmation(
            "the out-of-office reply's text was stored, but switching it on or off was not; \
             read it again before writing again",
        )
    };
    let response = executor.execute(&request).await.map_err(|_| half_done())?;
    let result = response.result(&set).map_err(|_| half_done())?;
    if result
        .get("updated")
        .and_then(|updated| updated.get(running.id.as_str()))
        .is_none()
    {
        return Err(half_done());
    }
    let version = result
        .get("newState")
        .and_then(Value::as_str)
        .map(VacationVersion::new);
    Ok(VacationReceipt::new(version, RulesEffect::Untouched))
}

#[cfg(test)]
#[path = "vacation_include_tests.rs"]
mod tests;
