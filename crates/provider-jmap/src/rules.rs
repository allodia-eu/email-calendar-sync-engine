//! The account's mail rules over JMAP Sieve (RFC 9661), as the neutral [`MailRules`]
//! (`filters-and-out-of-office.md`, slice 2).
//!
//! - **Reached only where the account has it.** [`reachable`] is the session's answer for
//!   `urn:ietf:params:jmap:sieve`, so a client bound to a read-only share has no rules.
//! - **One script holds the rules** ([`Scripts::rules`]): the one running, else the engine's own
//!   `rules` script or the one script besides the reply's; with none, a write creates `rules` and
//!   activates it. It is read and written in Roundcube's dialect (`engine_sieve::dialect`): a rule
//!   the caller did not change is copied byte for byte.
//! - **Planned first.** `plan_rules` in edit mode decides what the account's extensions hold
//!   (`engine_sieve::dialect::vocabulary`, from the session); the receipt carries its outcomes.
//! - **Guarded.** The write sends `ifInState` with the version read (the `SieveScript` type's
//!   state), so a stale one is `stateMismatch`, a `Conflict`. The script is uploaded first and then
//!   stored, created or updated, and activated in one `SieveScript/set`, which refuses an invalid
//!   script whole: nothing stored, nothing activated, the state unmoved (proven live).
//!   `SieveScript/validate` is not sent first: it adds a round trip and no safety, and on Stalwart
//!   cannot see a blob uploaded in the same request (`docs/spec-deviations.md`).
//! - **The reply keeps running.** Where the reply runs ([`reply_runs`]), the written script
//!   includes the reply's first, so activating the rules never switches the reply off.
//! - **No content in errors.** A refusal names its `SetError` type, never the server's description,
//!   which quotes the script.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawRules,
    rules::{MailRule, PlanMode, RuleSet, RuleVocabulary, RulesReceipt, plan_rules},
    version::RuleSetVersion,
};
use engine_provider::{MailRules, ProviderError, ProviderResult, WriteGuard, guard_version};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    error::{JmapError, set_error_type},
    executor::Executor,
    request::{Request, capability},
    session::Session,
    session_accounts::JmapAccountId,
    sieve_scripts::{self, RULES_SCRIPT, Scripts, reply_runs},
};

/// Whether the session gives this client an account with Sieve scripts.
pub(crate) fn reachable(session: &Session) -> bool {
    session.extension_account(capability::SIEVE).is_some()
}

/// The account Sieve calls address.
pub(crate) fn sieve_account(session: &Session) -> Result<&JmapAccountId, JmapError> {
    session.extension_account(capability::SIEVE).ok_or_else(|| {
        JmapError::session("this account has no Sieve scripts (no sieve capability)")
    })
}

/// What the account's extensions let the dialect write.
fn vocabulary(session: &Session) -> RuleVocabulary {
    let sieve = session.sieve.clone().unwrap_or_default();
    engine_sieve::dialect::vocabulary(
        sieve.extensions.iter().map(String::as_str),
        sieve.max_redirects,
    )
}

#[async_trait]
impl MailRules for JmapProvider {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(vocabulary(self.executor().session()))
    }

    /// `ifInState` on the `SieveScript` type: the rules are one script, so the type's state is
    /// their version, and a stale one is refused (proven live, `tests/live_rules.rs`).
    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        Ok(read(self.executor()).await?)
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        let guard = guard_version(base, self.rules_write_guard())?;
        write(self.executor(), guard, base, rules).await
    }
}

/// The scripts as listed now.
pub(crate) async fn list(executor: &dyn Executor) -> Result<Scripts, JmapError> {
    let account = sieve_account(executor.session())?;
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let call = sieve_scripts::list_call(&mut request, account);
    let response = executor.execute(&request).await?;
    sieve_scripts::parse(response.result(&call)?)
}

/// The rules script's text, or empty where there is none.
pub(crate) async fn rules_text(
    executor: &dyn Executor,
    scripts: &Scripts,
) -> Result<String, JmapError> {
    match scripts.rules() {
        Some(script) => {
            let account = sieve_account(executor.session())?;
            sieve_scripts::download(executor, account, &script.blob).await
        }
        None => Ok(String::new()),
    }
}

async fn read(executor: &dyn Executor) -> Result<RuleSet, JmapError> {
    let scripts = list(executor).await?;
    let text = rules_text(executor, &scripts).await?;
    Ok(RuleSet::new(sieve_scripts::user_rules(&text))
        .with_version(RuleSetVersion::new(scripts.state))
        .with_raw(RawRules::new(text)))
}

async fn write(
    executor: &dyn Executor,
    guard: Option<&RuleSetVersion>,
    base: &RuleSet,
    rules: &[MailRule],
) -> ProviderResult<RulesReceipt> {
    let session = executor.session();
    let account = sieve_account(session)?;
    let plan = plan_rules(rules, &vocabulary(session), PlanMode::Edit);

    let scripts = list(executor).await?;
    if guard.is_some_and(|version| version.as_str() != scripts.state) {
        return Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ));
    }
    let base_text = base.raw.as_ref().map_or("", RawRules::as_str);
    let running = scripts.rules_running().is_some().then_some(base_text);
    let reply_on = reply_runs(&scripts, running);
    let text = sieve_scripts::compose(base_text, &plan.written(), reply_on).map_err(|err| {
        ProviderError::item_unavailable(format!("the rules cannot be written as Sieve: {err}"))
    })?;
    if let Some(max) = session
        .sieve
        .as_ref()
        .and_then(|sieve| sieve.max_size_script)
        && text.len() > max
    {
        return Err(ProviderError::item_unavailable(format!(
            "the rules come to {} bytes of Sieve, over this account's {max}-byte limit",
            text.len()
        )));
    }
    let blob = sieve_scripts::upload(executor, account, &text).await?;

    let target = scripts.rules();
    let mut arguments = match target {
        Some(script) => json!({ "update": { script.id.as_str(): { "blobId": blob.as_str() } } }),
        None => json!({ "create": { "r": { "name": RULES_SCRIPT, "blobId": blob.as_str() } } }),
    };
    arguments["accountId"] = json!(account);
    if let Some(version) = guard {
        arguments["ifInState"] = json!(version.as_str());
    }
    match target {
        Some(script) if script.active => {}
        Some(script) => arguments["onSuccessActivateScript"] = json!(script.id),
        None => arguments["onSuccessActivateScript"] = json!("#r"),
    }
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let set = request.invoke("SieveScript/set", arguments);
    let response = executor
        .execute(&request)
        .await
        .map_err(JmapError::after_write)?;
    let result = response.result(&set).map_err(JmapError::after_write)?;
    let (done, refused, key) = match target {
        Some(script) => ("updated", "notUpdated", script.id.as_str()),
        None => ("created", "notCreated", "r"),
    };
    if let Some(refusal) = result.get(refused).and_then(|map| map.get(key)) {
        return Err(refused_script(key, refusal));
    }
    if result.get(done).and_then(|map| map.get(key)).is_none() {
        return Err(JmapError::unanswered(key, done, refused).into());
    }
    let version = result
        .get("newState")
        .and_then(Value::as_str)
        .map(RuleSetVersion::new);
    Ok(RulesReceipt::new(version, plan.outcomes))
}

/// A refused script: invalid Sieve (`invalidSieve`, RFC 9661 §2.4; Stalwart answers
/// `invalidScript`, `docs/spec-deviations.md`) will never be stored as it stands; anything else
/// keeps its class. The server's description quotes the script, so it is never carried.
pub(crate) fn refused_script(key: &str, refusal: &Value) -> ProviderError {
    match set_error_type(refusal) {
        kind @ ("invalidSieve" | "invalidScript") => ProviderError::item_unavailable(format!(
            "the server refused the rules script as invalid Sieve ({kind})"
        )),
        _ => JmapError::refused(key, refusal).into(),
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
