//! The account's mail rules over JMAP Sieve (RFC 9661), as the neutral [`MailRules`]
//! (`filters-and-out-of-office.md`, slice 2).
//!
//! - **Reached only where the account has it.** [`reachable`] is the session's answer for
//!   `urn:ietf:params:jmap:sieve`, so a client bound to a read-only share has no rules.
//! - **One script holds the rules** ([`Scripts::rules`]): the one running, else the engine's own
//!   `rules` script or the one script besides the reply's. It is written in place, running or not
//!   as it was ([`RuleSet::activation`]); only where there is none does a write create `rules` and
//!   activate it. It is read and written in Roundcube's dialect (`engine_sieve::dialect`): a rule
//!   the caller did not change is copied byte for byte.
//! - **Planned first.** `plan_rules` in edit mode decides what the account's extensions hold
//!   (`engine_sieve::dialect::vocabulary`, from the session); the receipt carries its outcomes.
//! - **Guarded.** The write sends `ifInState` with the version read (the `SieveScript` type's
//!   state), so a stale one is `stateMismatch`, a `Conflict`. The script is uploaded first and then
//!   stored, created or updated, and activated in one `SieveScript/set`, which refuses an invalid
//!   script whole: nothing stored, nothing activated, the state unmoved (proven live).
//!   `SieveScript/validate` is not sent first: it adds a round trip and no safety, and on Stalwart
//!   cannot see a blob uploaded in the same request (`docs/spec-deviations.md`, STALWART-6).
//! - **The reply keeps running where it can.** Where the reply runs ([`reply_runs`]) and the
//!   condition holds ([`Listing::include_usable`]), the written script includes the reply's first;
//!   where it does not and the write would stop the reply, the receipt says
//!   [`ReplyEffect::Stopped`]. What runs now, and what is copied unchanged, come from the server's
//!   script: `base`'s raw form only where `base` was read at the state just listed.
//! - **No content in errors.** A refusal names its `SetError` type, never the server's description,
//!   which quotes the script.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawRules,
    rules::{
        MailRule, PlanMode, ReplyEffect, RuleSet, RuleVocabulary, RulesActivation, RulesReceipt,
        RulesStored, plan_rules,
    },
    version::RuleSetVersion,
};
use engine_provider::{
    MailRules, ProviderError, ProviderResult, TargetRefusal, WriteGuard, guard_version,
};
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

/// The scripts as listed now, and, where the account has a reply, its state read in the same
/// request ([`sieve_scripts::include_usable`]).
pub(crate) struct Listing {
    pub(crate) scripts: Scripts,
    pub(crate) reply_state: Option<String>,
}

impl Listing {
    /// Whether the reply may run through the rules' `include` ([`sieve_scripts::include_usable`]).
    pub(crate) fn include_usable(&self, session: &Session) -> bool {
        sieve_scripts::include_usable(session, &self.scripts.state, self.reply_state.as_deref())
    }
}

/// The scripts as listed now, with the reply's state beside them.
pub(crate) async fn list(executor: &dyn Executor) -> Result<Listing, JmapError> {
    let session = executor.session();
    let account = sieve_account(session)?;
    let reply = session.extension_account(capability::VACATION_RESPONSE);
    let mut using = vec![capability::CORE, capability::SIEVE];
    if reply.is_some() {
        using.push(capability::VACATION_RESPONSE);
    }
    let mut request = Request::new(using);
    let call = sieve_scripts::list_call(&mut request, account);
    let state = reply.map(|reply| {
        request.invoke(
            "VacationResponse/get",
            json!({ "accountId": reply, "ids": ["singleton"], "properties": ["id"] }),
        )
    });
    let response = executor.execute(&request).await?;
    let scripts = sieve_scripts::parse(response.result(&call)?)?;
    let reply_state = state
        .and_then(|call| response.result(&call).ok())
        .and_then(|result| result.get("state")?.as_str().map(str::to_owned));
    Ok(Listing {
        scripts,
        reply_state,
    })
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
    let Listing { scripts, .. } = list(executor).await?;
    let text = rules_text(executor, &scripts).await?;
    let activation = match scripts.rules() {
        Some(script) if !script.active => RulesActivation::Inactive,
        _ => RulesActivation::Running,
    };
    Ok(RuleSet::new(sieve_scripts::user_rules(&text))
        .with_version(RuleSetVersion::new(scripts.state))
        .with_raw(RawRules::new(text))
        .with_activation(activation))
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

    let listing = list(executor).await?;
    let scripts = &listing.scripts;
    if guard.is_some_and(|version| version.as_str() != scripts.state) {
        return Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ));
    }
    // What runs now, and what is copied unchanged, come from the server's script: `base`'s raw
    // form stands for it only where `base` was read at the state just listed.
    let current = base.version.as_ref().map(RuleSetVersion::as_str) == Some(scripts.state.as_str());
    let base_text = match &base.raw {
        Some(raw) if current => raw.as_str().to_owned(),
        _ => rules_text(executor, scripts).await?,
    };
    let target = scripts.rules();
    let running = target.is_some_and(|script| script.active);
    let reply_ran = reply_runs(scripts, running.then_some(base_text.as_str()));
    // Keeping an include the running script has needs nothing more; adding one needs the
    // condition. So the write keeps exactly what a read reports (`reply_runs`).
    let include =
        reply_ran && (sieve_scripts::includes_reply(&base_text) || listing.include_usable(session));
    let text = sieve_scripts::compose(&base_text, &plan.written(), include).map_err(|err| {
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

    // Written in place, running or not as it was; only an account with no script to edit gets
    // one, made the active one.
    let mut arguments = match target {
        Some(script) => json!({ "update": { script.id.as_str(): { "blobId": blob.as_str() } } }),
        None => json!({
            "create": { "r": { "name": RULES_SCRIPT, "blobId": blob.as_str() } },
            "onSuccessActivateScript": "#r",
        }),
    };
    arguments["accountId"] = json!(account);
    if let Some(version) = guard {
        arguments["ifInState"] = json!(version.as_str());
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
    // The reply stops where it ran through what this write replaced or switched off, and the
    // written script could not include it.
    let reply_script_runs = scripts.reply().is_some_and(|reply| reply.active) && target.is_some();
    let reply = if reply_ran && !include && !reply_script_runs {
        ReplyEffect::Stopped
    } else {
        ReplyEffect::Untouched
    };
    let stored = if target.is_some() {
        RulesStored::InPlace
    } else {
        RulesStored::CreatedRunning
    };
    // Written where they were, running or not as they were; a script the write created runs.
    let activation = if target.is_none_or(|script| script.active) {
        RulesActivation::Running
    } else {
        RulesActivation::Inactive
    };
    Ok(RulesReceipt::new(version, plan.outcomes)
        .with_stored(stored)
        .with_reply(reply)
        .with_activation(activation))
}

/// A refused script. Invalid Sieve (`invalidSieve`, RFC 9661 §2.4; Stalwart answers
/// `invalidScript`, `docs/spec-deviations.md`, STALWART-5) or a script over the size limit
/// (`tooLarge`) will never be stored as it stands: `item_unavailable`. `overQuota` (RFC 9661 §2.4:
/// the script count or the account's storage) is the target's own state, which waiting does not
/// clear: `refused_by_target(StorageFull)`. Anything else keeps its class. The server's description
/// quotes the script, so it is never carried.
pub(crate) fn refused_script(key: &str, refusal: &Value) -> ProviderError {
    match set_error_type(refusal) {
        kind @ ("invalidSieve" | "invalidScript") => ProviderError::item_unavailable(format!(
            "the server refused the rules script as invalid Sieve ({kind})"
        )),
        "tooLarge" => ProviderError::item_unavailable(
            "the server refused the rules script as too large (tooLarge)",
        ),
        "overQuota" => ProviderError::refused_by_target(TargetRefusal::StorageFull),
        _ => JmapError::refused(key, refusal).into(),
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
