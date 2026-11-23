//! The account's mail rules over ManageSieve (RFC 5804), as the neutral [`MailRules`]
//! (`filters-and-out-of-office.md`, slice 3).
//!
//! - **One script holds the rules and the reply** ([`State`]): the active one, else `rules`, else
//!   the only one. It is read and written in Roundcube's dialect (`engine_sieve::dialect`), every
//!   rule the caller did not change copied byte for byte, and the out-of-office reply's rule kept
//!   where it is: it is the reply's, not one of the rules (`crate::managesieve::vacation`).
//! - **Planned first.** `plan_rules` in edit mode decides what the account's extensions hold, from
//!   the `SIEVE` capability the server lists once signed in; the receipt carries its outcomes.
//! - **Guarded as far as ManageSieve allows.** It has no conditional `PUTSCRIPT`, so the guard is
//!   [`WriteGuard::Absent`]. A write built on a read still lists and reads the scripts again in its
//!   own session and refuses as `Conflict` when the version moved ([`State::version`]): that
//!   narrows the race with another client to one round trip, and does not close it.
//! - **Stored, then switched on only where new.** `PUTSCRIPT` checks the script and stores nothing
//!   it refuses (RFC 5804 §2.6), so `CHECKSCRIPT` is not sent first, and `HAVESPACE` neither: a
//!   script too large is refused by the store itself. Only a script the write created is made the
//!   active one ([`RulesStored::CreatedRunning`]).
//! - **No content in errors.** A refusal carries its response code, never the server's text, which
//!   quotes the script.

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
use engine_provider::{MailRules, ProviderError, ProviderResult, WriteGuard};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    error::SieveError,
    name::ScriptName,
    response::{Capabilities, Quota, ResponseCode},
    service::ManageSieve,
    session::Session,
    state::State,
};

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> MailRules for ManageSieve<T> {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        self.with_session(async |session: &mut Session<T>| {
            Ok(vocabulary(session.refresh_capabilities().await?))
        })
        .await
    }

    /// [`WriteGuard::Absent`]: ManageSieve has no conditional `PUTSCRIPT`. The write still
    /// refuses a base whose version moved, as read again in its own session, so only a change
    /// landing between that read and the store is lost ([module docs](self)).
    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        self.with_session(async |session: &mut Session<T>| {
            let state = State::read(session).await?;
            Ok(rule_set(&state))
        })
        .await
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        self.with_session(async |session: &mut Session<T>| write(session, base, rules).await)
            .await
    }
}

/// What the account's extensions let the dialect write, from what the server lists once signed in
/// (Dovecot lists `MAXREDIRECTS` only then).
fn vocabulary(capabilities: &Capabilities) -> RuleVocabulary {
    engine_sieve::dialect::vocabulary(
        capabilities.sieve.iter().map(String::as_str),
        capabilities.max_redirects,
    )
}

fn rule_set(state: &State) -> RuleSet {
    let activation = if state.running() {
        RulesActivation::Running
    } else {
        RulesActivation::Inactive
    };
    RuleSet::new(engine_sieve::dialect::read_rules(&state.text))
        .with_version(RuleSetVersion::new(state.version()))
        .with_raw(RawRules::new(state.text.clone()))
        .with_activation(activation)
}

async fn write<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &mut Session<T>,
    base: &RuleSet,
    rules: &[MailRule],
) -> ProviderResult<RulesReceipt> {
    let plan = plan_rules(
        rules,
        &vocabulary(session.refresh_capabilities().await?),
        PlanMode::Edit,
    );
    let state = State::read(session).await?;
    refuse_stale(base.version.as_ref().map(RuleSetVersion::as_str), &state)?;
    let text = engine_sieve::dialect::write_rules(&state.text, &plan.written()).map_err(|err| {
        ProviderError::item_unavailable(format!("the rules cannot be written as Sieve: {err}"))
    })?;
    let created = store(session, &state, &text).await?;
    let version = State::read(session)
        .await
        .ok()
        .map(|after| RuleSetVersion::new(after.version()));
    let stored = if created {
        RulesStored::CreatedRunning
    } else {
        RulesStored::InPlace
    };
    Ok(RulesReceipt::new(version, plan.outcomes)
        .with_stored(stored)
        .with_reply(ReplyEffect::Untouched))
}

/// Refuses a write built on `read`, a version the script no longer has. `None`: a base built
/// rather than read, written as it stands (the guard is `Absent`).
pub(crate) fn refuse_stale(read: Option<&str>, state: &State) -> ProviderResult<()> {
    match read {
        Some(read) if read != state.version() => Err(ProviderError::conflict(
            "the Sieve script changed since it was read; read it again",
        )),
        _ => Ok(()),
    }
}

/// Stores `text` as the script holding the rules and the reply, and where the write created it,
/// makes it the active one. Answers whether it created it.
///
/// # Errors
///
/// Before the server answered the store: [`ProviderError::needs_confirmation`] when the session
/// failed once `PUTSCRIPT` went out, or after it stored the script and before it was switched on.
/// A refusal: [`ProviderError::item_unavailable`] for a script the server finds invalid (`NO`
/// with no code) or too large (`QUOTA/MAXSIZE`); the code's class otherwise
/// (`From<SieveError>`).
pub(crate) async fn store<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &mut Session<T>,
    state: &State,
    text: &str,
) -> ProviderResult<bool> {
    let target = state.target();
    session
        .put_script(&target, text)
        .await
        .map_err(refused_store)?;
    let created = state.script.is_none();
    if created {
        activate(session, &target).await?;
    }
    Ok(created)
}

/// Makes `target`, stored a moment ago, the active script.
pub(crate) async fn activate<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &mut Session<T>,
    target: &ScriptName,
) -> ProviderResult<()> {
    session.set_active(Some(target)).await.map_err(|err| {
        ProviderError::needs_confirmation(
            "the Sieve script was stored and may not have been switched on; read it again",
        )
        .with_source(err)
    })
}

/// A failed `PUTSCRIPT`, classified ([`store`]).
fn refused_store(err: SieveError) -> ProviderError {
    match err {
        SieveError::Refused { code: None, .. } => {
            ProviderError::item_unavailable("the server refused the Sieve script as invalid")
                .with_source(err)
        }
        SieveError::Refused {
            code: Some(ResponseCode::Quota(Quota::MaxSize)),
            ..
        } => ProviderError::item_unavailable("the server refused the Sieve script as too large")
            .with_source(err),
        SieveError::Refused { .. } => err.into(),
        // The command went out: the script may have been stored.
        SieveError::Imap(_) => ProviderError::needs_confirmation(
            "the Sieve script may have been stored; read it again",
        )
        .with_source(err),
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
