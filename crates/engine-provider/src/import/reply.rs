//! Importing an out-of-office reply: [`import_reply`], the one reply import every
//! [`RulesImporter`](super::RulesImporter) runs.

use engine_core::{
    ids::AccountId,
    time::UtcDateTime,
    vacation::{AutoReply, RulesEffect, VacationReceipt, VacationResponse, VacationWindow},
};

use crate::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, refuse_unheld_vacation,
};

/// Writes `reply` into `target` where it holds no reply of its own.
///
/// - A part the target cannot hold refuses the reply whole ([`refuse_unheld_vacation`], as an edit
///   is refused): dropping a window, an audience or an outside reply would widen who is answered.
/// - Where the target's reply is empty (off, with nothing to say and no window) it is written,
///   guarded by that read.
/// - Where it already says what `reply` says, as the target holds it, nothing is written, so an
///   import run again after an interruption answers as the first did. The two are compared with
///   each CRLF an LF and one line end closing a text dropped (a Sieve `text:` block adds it), and
///   the window to whole seconds inward, as a Sieve writer stores it.
/// - Any other reply, on or off, is the target's own: refused, never replaced. A host that means to
///   replace it writes through [`Vacation::set_vacation`] itself.
///
/// One race stays open. A target may refuse a write that would switch the account's running rules
/// off (the JMAP importer does), but it decides that from what it lists before it writes. On a
/// server that keeps the rules' and the reply's versions apart, a script of the user's switched on
/// between that listing and the write is switched off by a reply going on by the server's flag.
/// Nothing guards against it; the receipt says it happened
/// ([`RulesEffect::Deactivated`](engine_core::vacation::RulesEffect::Deactivated)), which is how
/// the host learns of it and can offer to switch the rules back on.
///
/// # Errors
///
/// [`RulesImporter::import_vacation`](super::RulesImporter::import_vacation)'s.
pub async fn import_reply(
    target: &dyn Vacation,
    account: &AccountId,
    reply: &VacationResponse,
) -> ProviderResult<VacationReceipt> {
    refuse_unheld_vacation(&target.vocabulary(account).await?, reply)?;
    let held = target.read_vacation(account).await?;
    if as_held(&held) == as_held(reply) {
        return Ok(VacationReceipt::new(held.version, RulesEffect::Untouched));
    }
    if as_held(&held) != as_held(&VacationResponse::default()) {
        return Err(ProviderError::item_unavailable(
            "the target already holds an out-of-office reply of its own, so the imported one is \
             not written over it",
        ));
    }
    target
        .set_vacation(account, VacationPrecondition::IfUnchanged(&held), reply)
        .await
}

/// What a reply says, as a target holds it: every modelled part, without the version and the raw
/// form it was read with, and with what a target's writer changes on the way made the same, so a
/// reply the import wrote reads as the one it was handed.
///
/// - **Line ends**: a CRLF is an LF, and one line end closing a text is dropped (a Sieve `text:`
///   block ends its value with one, RFC 5228 §2.4.2).
/// - **The window**, to whole seconds inward, as a Sieve writer stores it: the start up to the next
///   whole second, the end down to the last.
fn as_held(reply: &VacationResponse) -> VacationResponse {
    let mut held = VacationResponse::default()
        .with_window(whole_seconds(reply.window))
        .with_reply(texts(&reply.reply))
        .with_audience(reply.audience);
    if let Some(outside) = &reply.outside_reply {
        held = held.with_outside_reply(texts(outside));
    }
    if reply.enabled {
        held.turned_on()
    } else {
        held
    }
}

fn texts(reply: &AutoReply) -> AutoReply {
    let mut held = AutoReply::default();
    if let Some(subject) = &reply.subject {
        held = held.with_subject(text(subject));
    }
    if let Some(body) = &reply.text {
        held = held.with_text(text(body));
    }
    if let Some(body) = &reply.html {
        held = held.with_html(text(body));
    }
    held
}

/// `value` with each CRLF an LF, and one closing line end dropped.
fn text(value: &str) -> String {
    let lf = value.replace("\r\n", "\n");
    lf.strip_suffix('\n')
        .map_or_else(|| lf.clone(), str::to_owned)
}

fn whole_seconds(window: VacationWindow) -> VacationWindow {
    let mut held = VacationWindow::default();
    if let Some(start) = window.start {
        held = held.with_start(ceil(start));
    }
    if let Some(end) = window.end {
        held = held.with_end(floor(end));
    }
    held
}

/// `at`, its fraction of a second dropped.
fn floor(at: UtcDateTime) -> UtcDateTime {
    at.checked_sub(core::time::Duration::from_nanos(u64::from(at.nanosecond())))
        .unwrap_or(at)
}

/// The first whole second at or after `at`.
fn ceil(at: UtcDateTime) -> UtcDateTime {
    if at.nanosecond() == 0 {
        return at;
    }
    floor(at)
        .checked_add(core::time::Duration::from_secs(1))
        .unwrap_or(at)
}

#[cfg(test)]
#[path = "reply_tests.rs"]
mod tests;
