//! The out-of-office reply over ManageSieve, as the neutral [`Vacation`]
//! (`filters-and-out-of-office.md`, slice 3).
//!
//! - **A rule of the rules' script.** ManageSieve has no reply of its own, so the reply is the rule
//!   Roundcube's vacation engine writes, in the script that holds the rules
//!   (`engine_sieve::dialect::reply`). Rules and reply are one script with one version: a write of
//!   either is refused when the other moved since the read, as on JMAP's shared counter.
//! - **On means the rule is on and the script runs.** Turning the reply on where the script is
//!   stored but not active switches the script on, unless it holds enabled rules that do not run
//!   now: switching those on is the user's call, so that write is refused. The engine never
//!   switches a user's rules on as a side effect, as it never does on JMAP.
//! - **Guarded as far as ManageSieve allows** ([`WriteGuard::Absent`],
//!   `crate::managesieve::rules`). An [`Unconditional`](VacationPrecondition::Unconditional) write
//!   is the one that may replace a reply rule the reader does not read; a guarded one is refused
//!   there.
//! - **Text only for now.** The rule holds a subject and a text body; an HTML body is refused
//!   before sending, as a part the account cannot hold.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    vacation::{RulesEffect, VacationReceipt, VacationResponse, VacationVocabulary, WindowSupport},
    version::VacationVersion,
};
use engine_provider::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard,
    refuse_unheld_vacation,
};
use engine_sieve::dialect::{
    DialectError, DialectErrorKind, read_rules,
    reply::{ReplyRefusal, ScriptReply, read_reply, write_reply, write_reply_over},
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    response::Capabilities,
    rules::{activate, refuse_stale, store},
    service::ManageSieve,
    session::Session,
    state::State,
};

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Vacation for ManageSieve<T> {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        self.with_session(async |session: &mut Session<T>| {
            Ok(vocabulary(session.refresh_capabilities().await?))
        })
        .await
    }

    /// [`WriteGuard::Absent`], as for the rules they share a script with
    /// (`crate::managesieve::rules`): a stale read is refused, re-read in the write's own session,
    /// but a change landing in the one round trip before the store is not.
    fn vacation_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        self.with_session(async |session: &mut Session<T>| read(&State::read(session).await?))
            .await
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        self.with_session(async |session: &mut Session<T>| {
            refuse_unheld_vacation(&vocabulary(session.refresh_capabilities().await?), new)?;
            write(session, precondition, new).await
        })
        .await
    }
}

/// What the rule can hold: a subject and one text body, to everyone, and a window where the server
/// lists `date` and `relational`, the extensions its tests need.
fn vocabulary(capabilities: &Capabilities) -> VacationVocabulary {
    let window = if capabilities.lists("date") && capabilities.lists("relational") {
        WindowSupport::EitherEnd
    } else {
        WindowSupport::None
    };
    VacationVocabulary::everyone()
        .with_window(window)
        .with_subject()
}

/// The reply `state`'s script holds: off where there is none, or where the script holding it does
/// not run.
fn read(state: &State) -> ProviderResult<VacationResponse> {
    let version = VacationVersion::new(state.version());
    match read_reply(&state.text) {
        ScriptReply::Read { reply, .. } => {
            let reply = (*reply).with_version(version);
            Ok(if state.running() {
                reply
            } else {
                reply.turned_off()
            })
        }
        ScriptReply::Unreadable(_) => Err(ProviderError::item_unavailable(
            "this account's out-of-office reply is a Sieve rule the engine cannot read",
        )),
        _ => Ok(VacationResponse::default().with_version(version)),
    }
}

async fn write<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &mut Session<T>,
    precondition: VacationPrecondition<'_>,
    new: &VacationResponse,
) -> ProviderResult<VacationReceipt> {
    let state = State::read(session).await?;
    if let VacationPrecondition::IfUnchanged(read) = precondition {
        refuse_stale(read.version.as_ref().map(VacationVersion::as_str), &state)?;
    }
    let text = match (read_reply(&state.text), precondition) {
        (ScriptReply::Unreadable(unread), VacationPrecondition::Unconditional) => {
            write_reply_over(&state.text, new, &unread)
        }
        _ => write_reply(&state.text, new),
    }
    .map_err(unwritable)?;
    // On means the script runs too: switch it on only where that switches on no rule of the
    // user's that is off now.
    let switch_on = new.enabled && !state.running();
    if switch_on && read_rules(&state.text).iter().any(|rule| rule.enabled) {
        return Err(ProviderError::item_unavailable(
            "turning the out-of-office reply on would switch on the account's stored mail rules, \
             which do not run now",
        ));
    }
    let created = store(session, &state, &text).await?;
    if switch_on && !created {
        activate(session, &state.target()).await?;
    }
    let version = State::read(session)
        .await
        .ok()
        .map(|after| VacationVersion::new(after.version()));
    Ok(VacationReceipt::new(version, RulesEffect::Untouched))
}

/// Why the reply cannot be written as a rule, in the engine's words: never written, and never
/// will be as the reply and the script stand.
fn unwritable(err: DialectError) -> ProviderError {
    let why = match err.kind() {
        DialectErrorKind::Reply(ReplyRefusal::Html) => {
            "it has an HTML body, which the rule cannot hold"
        }
        DialectErrorKind::Reply(ReplyRefusal::NoText) => "it has no text to send",
        DialectErrorKind::Reply(ReplyRefusal::EmptyWindow) => {
            "its window holds no whole second, so it would never be sent"
        }
        DialectErrorKind::Reply(ReplyRefusal::OtherVacation) => {
            "another of the account's rules sends an automatic reply too, and two for one message \
             are an error"
        }
        DialectErrorKind::Reply(ReplyRefusal::Unread) => {
            "the account's reply rule is one the engine cannot read; write unconditionally to \
             replace it"
        }
        _ => "it holds what the Sieve rule cannot",
    };
    ProviderError::item_unavailable(format!("the out-of-office reply cannot be written: {why}"))
        .with_source(err)
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
