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
//! - **Only where the reply rule is the only reply.** A script that runs another through `include`,
//!   or holds a `vacation` outside the reply rule, may answer from there: a JMAP server that runs
//!   its own reply script (Stalwart's `vacation`, included by the rules or active itself) is one
//!   such account. The engine cannot read that reply as the account's, and writing one beside it
//!   could make two, so both the read and the write are refused ([`reply_elsewhere`]). The rules
//!   stay readable and writable.
//! - **Text only for now.** The rule holds a subject and a text body; an HTML body is refused
//!   before sending, as a part the account cannot hold.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    rules::RuleBody,
    vacation::{RulesEffect, VacationReceipt, VacationResponse, VacationVocabulary, WindowSupport},
    version::VacationVersion,
};
use engine_provider::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard,
    refuse_unheld_vacation,
};
use engine_sieve::dialect::{
    DialectError, DialectErrorKind, read_rules,
    reply::{ReplyRefusal, ScriptReply, includes, read_reply, write_reply, write_reply_over},
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    probe_state::Offer,
    response::Capabilities,
    rules::{activate, refuse_stale, store},
    service::ManageSieve,
    session::Session,
    state::State,
};

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Vacation for ManageSieve<T> {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        refuse_unoffered(self).await?;
        self.with_session(async |session: &mut Session<T>| {
            refuse_unlisted(session)?;
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
        refuse_unoffered(self).await?;
        self.with_session(async |session: &mut Session<T>| {
            refuse_unlisted(session)?;
            read(&State::read(session).await?)
        })
        .await
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        refuse_unoffered(self).await?;
        self.with_session(async |session: &mut Session<T>| {
            refuse_unlisted(session)?;
            refuse_unheld_vacation(&vocabulary(session.refresh_capabilities().await?), new)?;
            write(session, precondition, new).await
        })
        .await
    }
}

/// The words for a server that lists no `vacation`.
const UNLISTED: &str =
    "this account's Sieve server lists no vacation extension, so it keeps no reply";

/// Refuses before anything is dialled where the probe found no `vacation`: the reply is offered
/// while the account's first probe runs, before that is known (`crate::managesieve::reach`), and a
/// call waits for it, so a call made then never signs in to a server that keeps no reply.
async fn refuse_unoffered<T>(sieve: &ManageSieve<T>) -> ProviderResult<()> {
    match sieve.probe.settled().await {
        Offer::Found { reply: false } => Err(ProviderError::item_unavailable(UNLISTED)),
        _ => Ok(()),
    }
}

/// Refuses where the server lists no `vacation` once signed in: a server's list can change after
/// the probe read it, so each call checks.
fn refuse_unlisted<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: &Session<T>,
) -> ProviderResult<()> {
    if session.capabilities().lists("vacation") {
        return Ok(());
    }
    Err(ProviderError::item_unavailable(UNLISTED))
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
    refuse_elsewhere(state)?;
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
    refuse_elsewhere(&state)?;
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

/// Whether an automatic reply may run from somewhere other than `script`'s reply rule: another
/// script it runs through `include` (which the dialect does not read), or a `vacation` in a rule of
/// the user's, any branch, enabled or not (`read_rules` leaves the reply rule out). Stalwart's own
/// reply script, as its JMAP `VacationResponse` writes it, is the second: nested `currentdate`
/// tests around `vacation :mime`.
pub(crate) fn reply_elsewhere(script: &str) -> bool {
    includes(script)
        || read_rules(script).iter().any(|rule| match &rule.body {
            RuleBody::Opaque(raw) => engine_sieve::tokenize(raw.text()).map_or(true, |tokens| {
                tokens.iter().any(|token| token.is_identifier("vacation"))
            }),
            _ => false,
        })
}

/// Refuses to read or write the reply where it may run from elsewhere: a server's own reply
/// script runs (active, or included), or the rules' script may answer from another
/// ([`reply_elsewhere`]).
fn refuse_elsewhere(state: &State) -> ProviderResult<()> {
    if state.server_reply.is_some() || reply_elsewhere(&state.text) {
        return Err(ProviderError::item_unavailable(
            "an automatic reply may run from a script or rule other than the out-of-office reply's, \
             which the engine does not read; it neither reads nor writes the reply here",
        ));
    }
    Ok(())
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
