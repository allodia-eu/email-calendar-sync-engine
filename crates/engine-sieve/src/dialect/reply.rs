//! The out-of-office reply as a rule of the script, in the shape Roundcube's vacation engine
//! writes it.
//!
//! Where a server has no reply of its own (ManageSieve has none), the reply is a rule of the
//! script that holds the user's rules: Roundcube takes as the reply the first rule whose first
//! action is `vacation`, and writes a new one first. Its window is `currentdate` tests on the
//! `iso8601` part in an explicit zone, `allof` for two and `true` for none, and it is switched off
//! as any rule is (`if false # <window>`). The bytes are pinned from the plugin's own output
//! (`tests/fixtures/roundcube/reply_*.sieve`).
//!
//! - [`read_reply`] finds the reply rule and reads it into the neutral [`VacationResponse`]: on or
//!   off, its window, its subject and its text. A reply rule holding more than that (a test besides
//!   the window, an action besides `vacation`, a `:mime` body, a date in the server's own zone) is
//!   [`ScriptReply::Unreadable`]: read as anything less, it would answer people or do things the
//!   read does not say.
//! - [`write_reply`] writes a reply over a script: in place of the reply rule where there is one,
//!   keeping its name, the comments before it and the `vacation` arguments the model leaves to the
//!   script (`:days`, `:seconds`, `:from`, `:addresses`, `:handle`), and first where there is none,
//!   named `Out-of-Office` as the plugin names one. Every other rule is copied byte for byte
//!   ([`write_rules`](super::write_rules)).
//!
//! The window is written to the second: a reply starts at the first whole second at or after its
//! start and stops after the last whole second before its end, an end the plugin writes with
//! `le`. Its text is written with CRLF line ends, as every Sieve string holds them, and read back
//! with LF.

use core::fmt::Write as _;

use engine_core::{
    raw::RawVacation,
    rules::{Expansion, MailRule, RawRule, RuleName, RuleSyntax},
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, VacationResponse, VacationWindow},
};

use super::{
    DialectError, read,
    reply_read::{self, Kept, ReadReply, Shape},
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
    /// No rule of the script replies.
    Absent,
    /// The reply rule, read. `enabled` says whether the rule is on; whether the script runs is
    /// the server's to say. Its raw form is the rule's text.
    Read {
        /// The reply.
        reply: Box<VacationResponse>,
        /// The segment the rule is, as [`read_rules`](super::read_rules) numbers its origin.
        segment: usize,
    },
    /// A rule replies, in a shape the reader does not read ([module docs](self)).
    Unreadable {
        /// The segment the rule is.
        segment: usize,
    },
}

impl ScriptReply {
    /// The segment of the reply rule, where there is one: the rule a rule list read from the
    /// script leaves to the reply.
    #[must_use]
    pub fn segment(&self) -> Option<usize> {
        match self {
            Self::Absent => None,
            Self::Read { segment, .. } | Self::Unreadable { segment } => Some(*segment),
        }
    }
}

/// The reply `script` holds ([module docs](self)). A script that cannot be split at its commands
/// holds none the reader can find.
#[must_use]
pub fn read_reply(script: &str) -> ScriptReply {
    let Ok(segments) = crate::segment(script) else {
        return ScriptReply::Absent;
    };
    match found(&segments) {
        None => ScriptReply::Absent,
        Some((segment, Shape::Read(read))) => ScriptReply::Read {
            reply: Box::new(neutral(read, segments[segment].text())),
            segment,
        },
        Some((segment, _)) => ScriptReply::Unreadable { segment },
    }
}

/// The first segment that is the reply, and its shape.
fn found(segments: &[Segment<'_>]) -> Option<(usize, Shape)> {
    segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.kind() == SegmentKind::Command)
        .map(|(index, segment)| (index, reply_read::shape(segment.tokens())))
        .find(|(_, shape)| !matches!(shape, Shape::Other))
}

fn neutral(read: ReadReply, text: &str) -> VacationResponse {
    let mut window = VacationWindow::default();
    if let Some(start) = read.start {
        window = window.with_start(start);
    }
    if let Some(end) = read.end {
        window = window.with_end(end);
    }
    let mut reply = AutoReply::default().with_text(read.text);
    if let Some(subject) = read.subject {
        reply = reply.with_subject(subject);
    }
    let response = VacationResponse::default()
        .with_window(window)
        .with_reply(reply)
        .with_raw(RawVacation::new(text));
    if read.enabled {
        response.turned_on()
    } else {
        response.turned_off()
    }
}

/// `reply` written over `base` ([module docs](self)): the script with its reply rule replaced,
/// or a new one first.
///
/// # Errors
///
/// A [`DialectError`]: `base` cannot be split at its commands, its `require` cannot be written,
/// or `reply` holds what the rule cannot ([`DialectErrorKind::Reply`](super::DialectErrorKind)):
/// an HTML body, an audience narrower than everyone, an outside reply, no text, or a value no
/// Sieve string holds; a value with `${` over a script that would expand it is
/// [`DialectErrorKind::Variable`](super::DialectErrorKind).
pub fn write_reply(base: &str, reply: &VacationResponse) -> Result<String, DialectError> {
    let segments = crate::segment(base).map_err(DialectError::unsplittable)?;
    let existing = found(&segments);
    let (kept, leading, name) = match &existing {
        Some((at, shape)) => {
            let segment = &segments[*at];
            let kept = match shape {
                Shape::Read(read) => read.kept.clone(),
                _ => Kept::default(),
            };
            let leading = read::without_names(segment.leading());
            (kept, leading, read::name(segment.leading()))
        }
        None => (Kept::default(), String::new(), None),
    };
    let name = name.unwrap_or_else(|| RuleName::new(REPLY_RULE_NAME).expect("a valid name"));
    let (text, needs) = render(reply, &kept, &leading, &name).ok_or_else(DialectError::reply)?;
    let mut raw = RawRule::new(RuleSyntax::Sieve, text)
        .requiring(needs.into_iter().map(str::to_owned))
        .sending_no_mail()
        .named(name.clone());
    if raw.text().contains("${") {
        raw = raw.holding(Expansion::Literal);
    }
    let rule = MailRule::opaque(raw).named(name);

    let mut rules = super::read_rules(base);
    match existing {
        Some((at, _)) => {
            let position = rules.iter().position(|rule| {
                matches!(rule.origin,
                    Some(engine_core::rules::RuleOrigin::ScriptSegment { index, .. }) if index == at)
            });
            match position {
                Some(position) => rules[position] = rule,
                None => rules.insert(0, rule),
            }
        }
        None => rules.insert(0, rule),
    }
    super::write_rules(base, &rules)
}

/// The reply rule's text and what it requires, or `None` where the rule cannot hold `reply`.
fn render(
    reply: &VacationResponse,
    kept: &Kept,
    leading: &str,
    name: &RuleName,
) -> Option<(String, Vec<&'static str>)> {
    let held = reply.audience == ReplyAudience::Everyone
        && reply.outside_reply.is_none()
        && reply.reply.html.is_none();
    let text = reply.reply.text.as_deref().filter(|_| held)?;
    let mut needs = vec!["vacation"];
    let mut tests = Vec::new();
    if let Some(start) = reply.window.start {
        tests.push(bound("ge", ceil(start)?));
    }
    if let Some(end) = reply.window.end {
        tests.push(bound(
            "le",
            ceil(end)?.checked_sub(core::time::Duration::from_secs(1))?,
        ));
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
    out.push_str("# rule:[");
    out.push_str(name.as_str());
    out.push_str("]\r\n");
    out.push_str(if reply.enabled { "if " } else { "if false # " });
    out.push_str(&condition);
    out.push_str("\r\n{\r\n\tvacation");
    if let Some(days) = kept.days {
        write!(out, " :days {days}").ok()?;
    }
    if let Some(seconds) = kept.seconds {
        write!(out, " :seconds {seconds}").ok()?;
    }
    if let Some(subject) = &reply.reply.subject {
        out.push_str(" :subject ");
        out.push_str(&value(subject)?);
    }
    if let Some(from) = &kept.from {
        out.push_str(" :from ");
        out.push_str(&value(from)?);
    }
    if let Some(addresses) = &kept.addresses {
        let rendered = addresses
            .iter()
            .map(|a| value(a))
            .collect::<Option<Vec<_>>>()?;
        write!(out, " :addresses [{}]", rendered.join(",")).ok()?;
    }
    if let Some(handle) = &kept.handle {
        out.push_str(" :handle ");
        out.push_str(&value(handle)?);
    }
    out.push(' ');
    out.push_str(&value(text)?);
    out.push_str(";\r\n}\r\n");
    Some((out, needs))
}

/// One bound of the window, as the plugin writes it.
fn bound(relation: &str, at: UtcDateTime) -> String {
    format!("currentdate :zone \"+0000\" :value \"{relation}\" \"iso8601\" \"{at}\"")
}

/// The first whole second at or after `at`.
fn ceil(at: UtcDateTime) -> Option<UtcDateTime> {
    let nanos = at.nanosecond();
    if nanos == 0 {
        return Some(at);
    }
    at.checked_sub(core::time::Duration::from_nanos(u64::from(nanos)))?
        .checked_add(core::time::Duration::from_secs(1))
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
