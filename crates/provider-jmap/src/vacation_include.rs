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
//! It is taken only where it holds (`jmap.md`, "the condition"), checked on every write: a script
//! of the user's runs, the account lists `include`, the `SieveScript` and `VacationResponse`
//! states read in one response are one counter, and the running script can be read and, where the
//! reply's state changes, rewritten. Otherwise the reply is written with the server's own flag,
//! as without rules, and the receipt says what that did to them; the engine never refuses the
//! reply for want of the include.
//!
//! Two requests, because Stalwart does not resolve a result reference for `ifInState`: it
//! ignored `#ifInState` pointing at the first call's `newState` even when that call failed
//! (`docs/spec-deviations.md`, STALWART-7). The second is guarded by the `newState` the first
//! answered, so nothing that landed between them is overwritten. If the first lands and the second
//! does not, the reply's words are stored and whether it runs is as it was: the write answers
//! [`requires_confirmation`](ProviderError::requires_confirmation), and the caller reads the
//! reply again before writing again.

use engine_core::{
    vacation::{RulesEffect, VacationReceipt},
    version::VacationVersion,
};
use engine_provider::{ProviderError, ProviderResult};
use serde_json::{Value, json};

use crate::{
    error::{JmapError, set_refusal},
    executor::Executor,
    request::{Request, capability},
    rules::{self, sieve_account},
    session_accounts::JmapAccountId,
    sieve_scripts::{self, ComposeError, Script, Scripts, includes_reply, reply_runs},
};

/// What the include path needs, read where it holds ([module docs](self)).
pub(crate) struct Prepared {
    scripts: Scripts,
    running: Script,
    text: String,
}

impl Prepared {
    /// The running script with the include for `enabled`: `None` where it already has it as
    /// `enabled` says, and no script needs writing.
    pub(crate) fn toggled(&self, enabled: bool) -> Result<Option<String>, ComposeError> {
        if includes_reply(&self.text) == enabled {
            return Ok(None);
        }
        let user = sieve_scripts::user_rules(&self.text);
        sieve_scripts::compose(&self.text, &user, enabled).map(Some)
    }
}

/// The include path's facts, where they hold: the account has Sieve, a script of the user's runs,
/// the condition holds, and that script can be read. `None` otherwise, and the reply is written
/// with the server's own flag.
pub(crate) async fn prepare(executor: &dyn Executor) -> Result<Option<Prepared>, JmapError> {
    let session = executor.session();
    if !rules::reachable(session) {
        return Ok(None);
    }
    let listing = rules::list(executor).await?;
    let Some(running) = listing.scripts.rules_running().cloned() else {
        return Ok(None);
    };
    if !listing.include_usable(session) {
        return Ok(None);
    }
    let account = sieve_account(session)?;
    let Ok(text) = sieve_scripts::download(executor, account, &running.blob).await else {
        return Ok(None);
    };
    Ok(Some(Prepared {
        scripts: listing.scripts,
        running,
        text,
    }))
}

/// Whether the reply runs through the running rules script, as `scripts` say: `false` where that
/// script cannot be read, so the reply reads as its own flag says.
pub(crate) async fn runs(executor: &dyn Executor, scripts: &Scripts) -> bool {
    let text = match scripts.rules_running() {
        Some(_) => match rules::rules_text(executor, scripts).await {
            Ok(text) => Some(text),
            Err(_) => return false,
        },
        None => None,
    };
    reply_runs(scripts, text.as_deref())
}

/// Writes `new` while the user's rules run ([module docs](self)): the words, then, where
/// `toggled` is a script, the include.
pub(crate) async fn write(
    executor: &dyn Executor,
    vacation_account: &JmapAccountId,
    guard: Option<&VacationVersion>,
    prepared: &Prepared,
    toggled: Option<String>,
    body: Value,
) -> ProviderResult<VacationReceipt> {
    let Prepared {
        scripts, running, ..
    } = prepared;
    if guard.is_some_and(|version| version.as_str() != scripts.state) {
        return Err(ProviderError::conflict(
            "the out-of-office reply changed since it was read; read it again",
        ));
    }
    let account = sieve_account(executor.session())?;
    let blob = match &toggled {
        Some(text) => Some(sieve_scripts::upload(executor, account, text).await?),
        None => None,
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
    let state = |key| result.get(key).and_then(Value::as_str);
    let written = state("newState")
        .ok_or_else(|| JmapError::unanswered("singleton", "updated", "notUpdated"))?
        .to_owned();
    let Some(blob) = blob else {
        return Ok(VacationReceipt::new(
            Some(VacationVersion::new(written)),
            RulesEffect::Untouched,
        ));
    };
    let half_done = || {
        ProviderError::needs_confirmation(
            "the out-of-office reply's text was stored, but switching it on or off was not; \
             read it again before writing again",
        )
    };
    // The include goes into the script as it was listed. If anything landed between that
    // listing and the words (an unguarded write does not check), that script may not be the
    // user's latest, so it is not written over.
    if state("oldState") != Some(scripts.state.as_str()) {
        return Err(half_done());
    }

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
