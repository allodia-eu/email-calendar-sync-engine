//! The out-of-office reply as a rule of the script, in the shape Roundcube's vacation engine
//! writes it.
//!
//! Where a server has no reply of its own (ManageSieve has none), the reply is a rule of the
//! script that holds the user's rules: the first rule whose first action is `vacation` and whose
//! tests are only those the plugin writes for a window (`reply_read` says why that narrowly). A
//! new one is written first, named `Out-of-Office` as the plugin names one. Its window is
//! `currentdate` tests on the `iso8601` part in an explicit zone, `allof` for two and `true` for
//! none, and it is switched off as any rule is (`if false # <window>`). The bytes are pinned from
//! the plugin's own output (`tests/fixtures/roundcube/reply_*.sieve`).
//!
//! The reply rule belongs to the reply alone: [`read_rules`](super::read_rules) leaves it out of
//! the user's rules, and [`write_rules`](super::write_rules) copies it where it was, byte for byte.
//!
//! - [`read_reply`] reads it into the neutral [`VacationResponse`]: on or off, its window, its
//!   subject and its text. A reply rule holding more (an action besides `vacation`, a `:mime` body,
//!   a date in the server's own zone, a `${` the script would expand) is
//!   [`ScriptReply::Unreadable`]: read as anything less, it would answer people or do things the
//!   read does not say.
//! - [`write_reply`] writes a reply over a script: in place of the reply rule, keeping its name,
//!   the comments before it and the `vacation` arguments the model leaves to the script (`:days`,
//!   `:seconds`, `:addresses`, `:handle`, `:from`); first where there is none. Every other rule is
//!   copied byte for byte. It refuses to write over an unread reply ([`ReplyRefusal::Unread`])
//!   unless the caller names it ([`write_reply_over`], which says what it keeps and what it
//!   discards), and a reply that is on where another rule also runs `vacation`
//!   ([`ReplyRefusal::OtherVacation`]): RFC 5230 §4.7 makes two an error, and Dovecot then sends
//!   neither.
//!
//! An unread reply belongs to neither list: [`read_rules`](super::read_rules) leaves its rule out,
//! and [`read_reply`] does not read it, so an action inside it, a redirect among them, is in
//! neither and [`plan_rules`](engine_core::rules::plan_rules) never sees it. A host shows
//! [`ScriptReply::Unreadable`] for what it is, and [`UnreadReply::sends_mail`] says whether it may
//! send mail out of the account, for a preflight to list.
//!
//! The window is written to the second and never wider than asked: a reply starts at the first
//! whole second at or after its start, and stops after the last whole second that ends by its
//! end, which the plugin writes with `le`. Its text is written with CRLF line ends, as every Sieve
//! string holds them, and read back with LF.

use core::fmt::Write as _;
use std::collections::BTreeSet;

use engine_core::{
    raw::RawVacation,
    rules::RuleName,
    time::UtcDateTime,
    vacation::{ReplyAudience, VacationResponse, VacationWindow},
};

use super::{
    DialectError, read,
    reply_read::{self, Found, Kept},
    write,
};
use crate::{
    segment::{Segment, SegmentKind},
    string,
};

/// The name a new reply rule gets: the plugin's own, from its `set_vacation`.
pub const REPLY_RULE_NAME: &str = "Out-of-Office";

/// The out-of-office reply a script holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptReply {
    /// No rule of the script is the reply.
    Absent,
    /// The reply rule, read. `enabled` says whether the rule is on; whether the script runs is
    /// the server's to say. Its raw form is the rule's text.
    Read {
        /// The reply.
        reply: Box<VacationResponse>,
        /// The segment the rule is.
        segment: usize,
    },
    /// The reply rule, in a shape the reader does not read ([module docs](self)). Writing over
    /// it takes this back ([`write_reply_over`]). What it does is in neither the reply nor the
    /// rules [`read_rules`](super::read_rules) reads, so a host must show it.
    Unreadable(UnreadReply),
}

/// A reply rule the reader does not read, as [`read_reply`] found it: what a caller hands back to
/// [`write_reply_over`] to say it means to replace that rule. Its `Debug` shows where the rule is
/// and how long, never its text.
#[derive(Clone, PartialEq, Eq)]
pub struct UnreadReply {
    segment: usize,
    /// The text of the rule, so the opt-in names this rule and no later one.
    text: String,
    sends_mail: bool,
    does_more: bool,
}

impl core::fmt::Debug for UnreadReply {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UnreadReply")
            .field("segment", &self.segment)
            .field("len", &self.text.len())
            .field("sends_mail", &self.sends_mail)
            .field("does_more", &self.does_more)
            .finish()
    }
}

impl UnreadReply {
    /// The segment the rule is.
    #[must_use]
    pub fn segment(&self) -> usize {
        self.segment
    }

    /// Whether the rule may send mail out of the account besides its reply: a `redirect`, a
    /// `notify`, a second `vacation`, or a command the reader does not know, after `vacation` or
    /// in an `elsif` or `else` after the block. A preflight lists it, since
    /// [`plan_rules`](engine_core::rules::plan_rules) never sees it.
    #[must_use]
    pub fn sends_mail(&self) -> bool {
        self.sends_mail
    }

    /// Whether the rule does anything besides `vacation`: commands after it in the block, or an
    /// `elsif` or `else` after the block, which [`write_reply_over`] keeps.
    #[must_use]
    pub fn does_more(&self) -> bool {
        self.does_more
    }
}

impl ScriptReply {
    /// The segment of the reply rule, where there is one.
    #[must_use]
    pub fn segment(&self) -> Option<usize> {
        match self {
            Self::Absent => None,
            Self::Read { segment, .. } => Some(*segment),
            Self::Unreadable(unread) => Some(unread.segment),
        }
    }
}

/// Why a reply cannot be written as a rule of the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplyRefusal {
    /// It has an HTML body: the rule holds text alone.
    Html,
    /// It answers fewer than everyone.
    Audience,
    /// It has a different reply for senders outside the organization.
    OutsideReply,
    /// It has no text to send.
    NoText,
    /// Its window holds no whole second: it would never be sent.
    EmptyWindow,
    /// Its text, or its subject, holds a character no Sieve string can: a CR outside a line end,
    /// or a NUL.
    ForbiddenCharacter,
    /// It holds `${` where the script requires `variables` or `encoded-character`, which would
    /// expand it.
    Expanded,
    /// It is on, and another rule of the script runs `vacation` too: RFC 5230 §4.7 makes two
    /// `vacation` actions for one message an error.
    OtherVacation,
    /// The script's reply rule is one the reader does not read, and the caller did not name it
    /// ([`write_reply_over`]), or named another.
    Unread,
}

/// The reply `script` holds ([module docs](self)). A script that cannot be split at its commands,
/// or names a malformed `require`, holds none the reader can find, and nothing is written over it.
#[must_use]
pub fn read_reply(script: &str) -> ScriptReply {
    let Ok(segments) = crate::segment(script) else {
        return ScriptReply::Absent;
    };
    let Ok(literal) = super::literal(&segments) else {
        return ScriptReply::Absent;
    };
    match reply_read::find(script, &segments, literal) {
        None => ScriptReply::Absent,
        // A `${` in the comments that name the rule would be refused by the writer: unread.
        Some((segment, found)) => match neutral(&found, segments[segment].text())
            .filter(|_| literal || !segments[segment].leading().contains("${"))
        {
            Some(reply) => ScriptReply::Read {
                reply: Box::new(reply),
                segment,
            },
            None => ScriptReply::Unreadable(UnreadReply {
                segment,
                text: segments[segment].text().to_owned(),
                sends_mail: found.sends_mail(),
                does_more: found.does_more(),
            }),
        },
    }
}

fn neutral(found: &Found, text: &str) -> Option<VacationResponse> {
    if !found.readable() {
        return None;
    }
    let ((start, end), action) = (found.window?, found.action.as_ref()?);
    let mut window = VacationWindow::default();
    if let Some(start) = start {
        window = window.with_start(start);
    }
    if let Some(end) = end {
        window = window.with_end(end);
    }
    let mut reply = engine_core::vacation::AutoReply::default().with_text(action.text.clone());
    if let Some(subject) = &action.subject {
        reply = reply.with_subject(subject.clone());
    }
    let response = VacationResponse::default()
        .with_window(window)
        .with_reply(reply)
        .with_raw(RawVacation::new(text));
    Some(if found.enabled {
        response.turned_on()
    } else {
        response.turned_off()
    })
}

/// `reply` written over `base` ([module docs](self)): the script with its reply rule replaced,
/// or a new one first.
///
/// # Errors
///
/// A [`DialectError`]: `base` cannot be split at its commands or its `require` cannot be
/// written; [`DialectErrorKind::Reply`](super::DialectErrorKind::Reply) with the
/// [`ReplyRefusal`] where the rule cannot hold `reply`, where `base`'s reply rule is one the
/// reader does not read ([`ReplyRefusal::Unread`]), or where `reply` is on and another rule runs
/// `vacation` ([`ReplyRefusal::OtherVacation`]).
pub fn write_reply(base: &str, reply: &VacationResponse) -> Result<String, DialectError> {
    write_over(base, reply, None)
}

/// [`write_reply`], replacing the unread reply rule `unread` names, as [`read_reply`] found it in
/// `base`: a deliberate overwrite.
///
/// Kept, as written: the rule's name and the comments before it; every command after `vacation`
/// in its block; an `elsif` or `else` after the block; and `vacation`'s `:days`, `:seconds`,
/// `:addresses`, `:handle` and `:from`, where `vacation` could be read. Replaced by `reply`: the
/// window, whether the rule is on, and `vacation`'s subject and text. Discarded: the window's
/// tests as they were, and all of `vacation`'s arguments where `vacation` could not be read
/// (a `:mime` body, a `${` the script would expand, an argument the reader does not know).
///
/// # Errors
///
/// As [`write_reply`]; [`ReplyRefusal::Unread`] where `base`'s reply rule is not the one
/// `unread` names.
pub fn write_reply_over(
    base: &str,
    reply: &VacationResponse,
    unread: &UnreadReply,
) -> Result<String, DialectError> {
    write_over(base, reply, Some(unread))
}

fn write_over(
    base: &str,
    reply: &VacationResponse,
    unread: Option<&UnreadReply>,
) -> Result<String, DialectError> {
    let segments = crate::segment(base).map_err(DialectError::unsplittable)?;
    let literal = super::literal(&segments).map_err(DialectError::unsplittable)?;
    let existing = reply_read::find(base, &segments, literal);
    let named = |at: usize| {
        unread.is_some_and(|unread| unread.segment == at && unread.text == segments[at].text())
    };
    let (found, leading, name) = match &existing {
        Some((at, found)) if found.readable() || named(*at) => {
            let segment = &segments[*at];
            let leading = read::without_names(segment.leading());
            (found.clone(), leading, read::name(segment.leading()))
        }
        Some(_) => return Err(DialectError::reply(ReplyRefusal::Unread)),
        None if unread.is_some() => return Err(DialectError::reply(ReplyRefusal::Unread)),
        None => (Found::default(), String::new(), None),
    };
    let name = name.unwrap_or_else(|| RuleName::new(REPLY_RULE_NAME).expect("a valid name"));
    let kept = found
        .action
        .as_ref()
        .map(|action| action.kept.clone())
        .unwrap_or_default();
    let reply_at = existing.as_ref().map(|(at, _)| *at);
    // A reply that is on, beside another `vacation` that may run for the same message.
    if reply.enabled && (another_vacation(&segments, reply_at) || found.holds_second_vacation()) {
        return Err(DialectError::reply(ReplyRefusal::OtherVacation));
    }
    let (text, needs) = render(reply, &kept, &found, &leading, &name)?;
    if !literal && text.contains("${") {
        return Err(DialectError::reply(ReplyRefusal::Expanded));
    }

    let mut pieces: Vec<(bool, String)> = segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.kind() == SegmentKind::Command)
        .map(|(at, segment)| match &existing {
            Some((reply_at, _)) if *reply_at == at => (true, text.clone()),
            _ => (false, segment.text().to_owned()),
        })
        .collect();
    if existing.is_none() {
        pieces.insert(0, (true, text));
    }
    let needed: BTreeSet<String> = needs.into_iter().map(str::to_owned).collect();
    write::assemble(&segments, &needed, pieces)
}

/// The reply rule's text and what it requires.
/// Whether a rule of the script other than the reply's runs `vacation`: one that is not disabled
/// (`if false # …`).
fn another_vacation(segments: &[Segment<'_>], reply: Option<usize>) -> bool {
    segments
        .iter()
        .enumerate()
        .filter(|(at, segment)| Some(*at) != reply && segment.kind() == SegmentKind::Command)
        .any(|(_, segment)| {
            let significant: Vec<_> = segment
                .tokens()
                .iter()
                .filter(|token| !token.kind().is_trivia())
                .collect();
            let disabled = matches!(significant.as_slice(), [first, second, ..]
                if first.is_identifier("if") && second.is_identifier("false"));
            !disabled && reply_read::holds_vacation(segment.tokens())
        })
}

fn render(
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
