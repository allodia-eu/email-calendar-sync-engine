//! The out-of-office reply rule, rendered as Roundcube's vacation engine writes it
//! (`super::reply`): the window to the whole second, `vacation`'s arguments in the plugin's order,
//! and what an overwritten rule keeps after `vacation`.

use core::fmt::Write as _;

use engine_core::{
    rules::RuleName,
    time::UtcDateTime,
    vacation::{ReplyAudience, VacationResponse, VacationWindow},
};

use super::{
    DialectError,
    reply::ReplyRefusal,
    reply_read::{Found, Kept},
};
use crate::string;

pub(super) fn render(
    reply: &VacationResponse,
    kept: &Kept,
    found: &Found,
    leading: &str,
    name: &RuleName,
) -> Result<(String, Vec<&'static str>), DialectError> {
    let refuse = |reason| Err(DialectError::reply(reason));
    if reply.reply.html.is_some() {
        return refuse(ReplyRefusal::Html);
    }
    if reply.audience != ReplyAudience::Everyone {
        return refuse(ReplyRefusal::Audience);
    }
    if reply.outside_reply.is_some() {
        return refuse(ReplyRefusal::OutsideReply);
    }
    let Some(text) = reply.reply.text.as_deref() else {
        return refuse(ReplyRefusal::NoText);
    };
    let (start, end) =
        window(&reply.window).ok_or(DialectError::reply(ReplyRefusal::EmptyWindow))?;
    let value =
        |text: &str| value(text).ok_or(DialectError::reply(ReplyRefusal::ForbiddenCharacter));

    // As the plugin requires: `vacation-seconds` in place of `vacation` where `:seconds` is used
    // (`rcube_sieve_script::as_text`), which RFC 6131 §2's capability implies.
    let mut needs = vec![if kept.seconds.is_some() {
        "vacation-seconds"
    } else {
        "vacation"
    }];
    let mut tests = Vec::new();
    if let Some(start) = start {
        tests.push(bound("ge", start));
    }
    if let Some(last) = end {
        tests.push(bound("le", last));
    }
    if !tests.is_empty() {
        needs.extend(["date", "relational"]);
    }
    let condition = match tests.as_slice() {
        [] => "true".to_owned(),
        [one] => one.clone(),
        many => format!("allof ({})", many.join(", ")),
    };
    let mut out = String::from(leading.trim_end_matches([' ', '\t']));
    let _ = write!(out, "# rule:[{}]\r\n", name.as_str());
    out.push_str(if reply.enabled { "if " } else { "if false # " });
    out.push_str(&condition);
    out.push_str("\r\n{\r\n\tvacation");
    // In the plugin's order (`rcube_sieve_script::as_text`): the interval, `:addresses`,
    // `:subject`, `:handle`, `:from`, then the reason.
    if let Some(seconds) = kept.seconds {
        let _ = write!(out, " :seconds {seconds}");
    } else if let Some(days) = kept.days {
        let _ = write!(out, " :days {days}");
    }
    if let Some(addresses) = &kept.addresses {
        let rendered = addresses
            .iter()
            .map(|address| value(address))
            .collect::<Result<Vec<_>, _>>()?;
        let _ = write!(out, " :addresses [{}]", rendered.join(","));
    }
    if let Some(subject) = &reply.reply.subject {
        let _ = write!(out, " :subject {}", value(subject)?);
    }
    if let Some(handle) = &kept.handle {
        let _ = write!(out, " :handle {}", value(handle)?);
    }
    if let Some(from) = &kept.from {
        let _ = write!(out, " :from {}", value(from)?);
    }
    let _ = write!(out, " {};\r\n", value(text)?);
    for other in &found.others {
        // A command that ended in a block ended there: a `;` after it would be a syntax error.
        let end = if other.ends_with('}') { "" } else { ";" };
        let _ = write!(out, "\t{other}{end}\r\n");
    }
    out.push('}');
    if found.after.is_empty() {
        out.push_str("\r\n");
    } else {
        // The `elsif` or `else` after the block, and the rest of its line, as written.
        out.push_str(&found.after);
        if !found.after.ends_with('\n') {
            out.push_str("\r\n");
        }
    }
    Ok((out, needs))
}

/// The whole seconds the window holds: the first, and the last (written with `le`), each where
/// the window has that end; `None` where it holds none.
fn window(window: &VacationWindow) -> Option<(Option<UtcDateTime>, Option<UtcDateTime>)> {
    let second = core::time::Duration::from_secs(1);
    let first = match window.start {
        Some(start) => Some(ceil(start)?),
        None => None,
    };
    let last = match window.end {
        Some(end) => Some(floor(end).checked_sub(second)?),
        None => None,
    };
    match (first, last) {
        (Some(first), Some(last)) if last < first => None,
        held => Some(held),
    }
}

/// `at`, its fraction of a second dropped.
fn floor(at: UtcDateTime) -> UtcDateTime {
    at.checked_sub(core::time::Duration::from_nanos(u64::from(at.nanosecond())))
        .unwrap_or(at)
}

/// The first whole second at or after `at`.
fn ceil(at: UtcDateTime) -> Option<UtcDateTime> {
    if at.nanosecond() == 0 {
        return Some(at);
    }
    floor(at).checked_add(core::time::Duration::from_secs(1))
}

/// One bound of the window, as the plugin writes it.
fn bound(relation: &str, at: UtcDateTime) -> String {
    format!("currentdate :zone \"+0000\" :value \"{relation}\" \"iso8601\" \"{at}\"")
}

/// A value as the plugin writes one: quoted on one line, a `text:` block across lines, every line
/// end a CRLF.
fn value(text: &str) -> Option<String> {
    let lf = text.replace("\r\n", "\n");
    if lf.contains('\r') {
        return None;
    }
    let crlf = lf.replace('\n', "\r\n");
    if crlf.contains('\n') {
        string::multi_line(&format!("{crlf}\r\n"))
    } else {
        string::quoted(&crlf)
    }
}
