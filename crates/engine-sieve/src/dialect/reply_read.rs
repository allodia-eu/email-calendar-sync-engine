//! The out-of-office reply rule: which rule it is, and what of it the model reads.
//!
//! Roundcube's vacation engine takes as the reply the first rule whose first action is `vacation`
//! (`rcube_sieve_vacation::vacation_rule`, `rcube_sieve_vacation.php:138-139` in 1.6.11), and on
//! saving drops from its tests exactly the ones it writes for the window: `currentdate`, `true`,
//! and `header :regex "received"`, its fallback without the `date` extension
//! (`rcube_sieve_vacation.php:259-263` and `:814-818`). Every other test it keeps, so a rule that
//! answers only some senders is, to the plugin, a reply with a narrower audience. The engine reads
//! no audience but everyone, so it narrows what counts as the reply instead: the first rule whose
//! first action is `vacation` **and** whose tests are only those the plugin writes for a window.
//! A user's own `if header :contains "from" "boss" { vacation …; }` stays an ordinary rule.
//!
//! Of the reply rule the model reads the window (`true`, or `currentdate :zone … :value …` tests,
//! `allof` for two), whether the rule is on, and `vacation`'s subject and text. What it leaves to
//! the script, `vacation`'s other arguments and any action after it, is kept for a rewrite. A rule
//! holding anything the model does not read is the reply, unread ([`Found::readable`]).

use engine_core::time::UtcDateTime;

use super::cursor::Cursor;
use crate::{
    lex::{Token, TokenKind, tokenize},
    segment::Segment,
};

/// A day, in seconds.
const DAY: u64 = 24 * 60 * 60;

/// The window: its start and its end, each where it has one.
pub(super) type Window = (Option<UtcDateTime>, Option<UtcDateTime>);

/// The reply rule, as far as it was read. No `Debug`: it holds the reply's words.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Found {
    /// Whether the rule is on (`if false # …` is off).
    pub(super) enabled: bool,
    /// The window, or `None` where its tests are not ones the model reads (a date in the server's
    /// own zone, Roundcube's `received` fallback).
    pub(super) window: Option<Window>,
    /// `vacation`'s arguments, or `None` where they hold what the model does not (`:mime`, a value
    /// the script's variables would expand).
    pub(super) action: Option<Action>,
    /// The commands after `vacation` in the rule's block, each as written without its `;`:
    /// Roundcube's own "then also" action (`discard`, `redirect`), or anything a user added.
    pub(super) others: Vec<String>,
    /// Whether the rule's shape is the one the model reads: one block, no `elsif` or `else`, no
    /// `${` the script would expand.
    pub(super) plain: bool,
}

impl Found {
    /// Whether the model reads the whole rule: a rule with less here would be read as a reply
    /// that answers or does less than it does.
    pub(super) fn readable(&self) -> bool {
        self.plain && self.window.is_some() && self.action.is_some() && self.others.is_empty()
    }
}

/// `vacation`'s arguments. No `Debug`: they hold the reply's words.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Action {
    pub(super) subject: Option<String>,
    /// The reason, its line ends as LF.
    pub(super) text: String,
    pub(super) kept: Kept,
}

/// `vacation`'s arguments the model leaves to the server or the script (decision 6 of the design
/// note: the interval is the server's), written back as they were read. No `Debug`: addresses.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) days: Option<u64>,
    pub(super) seconds: Option<u64>,
    pub(super) from: Option<String>,
    pub(super) addresses: Option<Vec<String>>,
    pub(super) handle: Option<String>,
}

/// The first segment of `segments` that is the reply rule, and what was read of it. `literal`:
/// whether the script's values read as written ([`super::literal`]).
pub(super) fn find(
    script: &str,
    segments: &[Segment<'_>],
    literal: bool,
) -> Option<(usize, Found)> {
    segments.iter().enumerate().find_map(|(index, segment)| {
        let found = reply(script, segment.tokens(), literal)?;
        Some((index, found))
    })
}

/// The reply rule `tokens` (one segment's) holds, or `None` where it is not the reply.
fn reply(script: &str, tokens: &[Token<'_>], literal: bool) -> Option<Found> {
    let significant: Vec<&Token<'_>> = tokens
        .iter()
        .filter(|token| token.kind() != TokenKind::Whitespace)
        .collect();
    if !significant.first()?.is_identifier("if") {
        return None;
    }
    let brace = significant
        .iter()
        .position(|token| token.kind() == TokenKind::LeftBrace)?;
    let close = matching_brace(&significant, brace)?;
    let block = commands(&significant[brace + 1..close]);
    let (first, rest) = block.split_first()?;
    if !first.first()?.is_identifier("vacation") {
        return None;
    }
    let commented;
    let (enabled, test): (bool, Vec<&Token<'_>>) = match &significant[1..brace] {
        [word, comment]
            if word.is_identifier("false") && comment.kind() == TokenKind::HashComment =>
        {
            commented = tokenize(comment.text()[1..].trim_start_matches(' ')).ok()?;
            (false, commented.iter().collect())
        }
        test => (true, test.to_vec()),
    };
    let test: Vec<&Token<'_>> = test
        .into_iter()
        .filter(|token| !token.kind().is_trivia())
        .collect();
    if !window_only(&test) {
        return None;
    }
    let after = &significant[close + 1..];
    Some(Found {
        enabled,
        window: window(&mut Cursor::new(test.iter().copied(), literal)),
        action: action(&mut Cursor::new(first[1..].iter().copied(), literal)),
        others: rest
            .iter()
            .map(|command| text_of(script, command))
            .collect(),
        plain: after.iter().all(|token| token.kind().is_trivia())
            && (literal || !text_of(script, &significant).contains("${")),
    })
}

/// Whether a test is made only of what the plugin writes for a window ([module docs](self)).
fn window_only(test: &[&Token<'_>]) -> bool {
    let regex_received = test.iter().any(|token| token.is_tag("regex"))
        && test.iter().any(|token| {
            token.kind() == TokenKind::QuotedString
                && token.text().eq_ignore_ascii_case("\"received\"")
        });
    !test.is_empty()
        && test
            .iter()
            .filter(|token| token.kind() == TokenKind::Identifier)
            .all(|token| {
                token.is_identifier("allof")
                    || token.is_identifier("currentdate")
                    || token.is_identifier("true")
                    || (token.is_identifier("header") && regex_received)
            })
}

/// The `}` closing the `{` at `open`.
fn matching_brace(tokens: &[&Token<'_>], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token.kind() {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// A block's commands, each its tokens without the `;` that ends it (a nested block ends one too),
/// comments left out. A `;` inside a list or a test list ends nothing.
fn commands<'t, 'a>(tokens: &[&'t Token<'a>]) -> Vec<Vec<&'t Token<'a>>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    for token in tokens.iter().copied().filter(|t| !t.kind().is_trivia()) {
        // A `;` ends a command only outside brackets, parentheses and braces, as RFC 5228 §8.1
        // has it; a `}` ends one that opened a block.
        match token.kind() {
            TokenKind::LeftBrace | TokenKind::LeftBracket | TokenKind::LeftParen => depth += 1,
            TokenKind::RightBrace | TokenKind::RightBracket | TokenKind::RightParen => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        let semicolon = depth == 0 && token.kind() == TokenKind::Semicolon;
        let block_end = depth == 0 && token.kind() == TokenKind::RightBrace;
        if !semicolon {
            current.push(token);
        }
        if semicolon || block_end {
            out.push(core::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// A command's text as written, from its first token to its last.
fn text_of(script: &str, command: &[&Token<'_>]) -> String {
    match (command.first(), command.last()) {
        (Some(first), Some(last)) => script[first.start()..last.end()].to_owned(),
        _ => String::new(),
    }
}

/// `true`, one `currentdate` test, or `allof` of tests each bounding one end once.
fn window(cursor: &mut Cursor<'_, '_>) -> Option<Window> {
    let token = cursor.next()?;
    let mut window = (None, None);
    if token.is_identifier("allof") {
        cursor.eat(TokenKind::LeftParen)?;
        loop {
            cursor.next()?.is_identifier("currentdate").then_some(())?;
            bound(cursor, &mut window)?;
            if cursor.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        cursor.eat(TokenKind::RightParen)?;
    } else if token.is_identifier("currentdate") {
        bound(cursor, &mut window)?;
    } else if !token.is_identifier("true") {
        return None;
    }
    cursor.done().then_some(window)
}

/// The rest of `currentdate :zone "<±hhmm>" :value "<rel>" "<part>" "<value>"`, tags in either
/// order, setting the end it bounds. Without `:zone` the date is the server's local one, which the
/// reader cannot know.
fn bound(cursor: &mut Cursor<'_, '_>, window: &mut Window) -> Option<()> {
    let (mut zone, mut relation) = (None, None);
    while let Some(tag) = cursor.tag() {
        match tag.as_str() {
            "zone" if zone.is_none() => zone = Some(offset(&cursor.string()?)?),
            "value" if relation.is_none() => relation = Some(cursor.string()?),
            _ => return None,
        }
    }
    let (zone, relation) = (zone?, relation?);
    let part = cursor.string()?;
    let value = cursor.string()?;
    // The first whole second the test holds at (a start), or the first it no longer does (an end).
    let (lower, at) = match part.as_str() {
        "iso8601" => {
            let at = UtcDateTime::parse_rfc3339(&value).ok()?;
            // The current date is rendered in `zone` and compared with the value as text, so only
            // a value written in that same zone compares as the instant it names. A zero offset is
            // read only as `Z`, the one spelling of it the plugin writes.
            if value_offset(&value)? != zone || at.nanosecond() != 0 {
                return None;
            }
            match relation.as_str() {
                "ge" => (true, at),
                "gt" => (true, at.checked_add(second())?),
                "le" => (false, at.checked_add(second())?),
                "lt" => (false, at),
                _ => return None,
            }
        }
        "date" => {
            let midnight = local_midnight(&value, zone)?;
            let next = midnight.checked_add(core::time::Duration::from_secs(DAY))?;
            match relation.as_str() {
                "ge" => (true, midnight),
                "gt" => (true, next),
                "le" => (false, next),
                "lt" => (false, midnight),
                _ => return None,
            }
        }
        _ => return None,
    };
    let end = if lower { &mut window.0 } else { &mut window.1 };
    if end.replace(at).is_some() {
        return None;
    }
    Some(())
}

fn second() -> core::time::Duration {
    core::time::Duration::from_secs(1)
}

/// A `:zone` argument, `+hhmm` or `-hhmm`, in seconds east of UTC.
fn offset(zone: &str) -> Option<i64> {
    let (sign, digits) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    (minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

/// The offset an RFC 3339 value is written in, in seconds east of UTC: `Z` for zero, `±hh:mm`
/// for any other. A zero written as `+00:00` is not taken.
fn value_offset(value: &str) -> Option<i64> {
    if value.ends_with('Z') {
        return Some(0);
    }
    let at = value.len().checked_sub(6)?;
    let sign = match value.as_bytes()[at] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = value.get(at + 1..)?.split_once(':')?;
    let seconds = sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60);
    (seconds != 0).then_some(seconds)
}

/// Midnight starting `date` (`YYYY-MM-DD`) in the zone `offset` seconds east of UTC.
fn local_midnight(date: &str, offset: i64) -> Option<UtcDateTime> {
    let wall = UtcDateTime::parse_rfc3339(&format!("{date}T00:00:00Z")).ok()?;
    let shift = core::time::Duration::from_secs(offset.unsigned_abs());
    if offset >= 0 {
        wall.checked_sub(shift)
    } else {
        wall.checked_add(shift)
    }
}

/// `vacation`'s arguments and reason (RFC 5230 §4, RFC 6131), as far as the model reads them.
fn action(cursor: &mut Cursor<'_, '_>) -> Option<Action> {
    let mut action = Action::default();
    let kept = &mut action.kept;
    while let Some(tag) = cursor.tag() {
        match tag.as_str() {
            "days" if kept.days.is_none() => kept.days = Some(cursor.number()?),
            "seconds" if kept.seconds.is_none() => kept.seconds = Some(cursor.number()?),
            "subject" if action.subject.is_none() => {
                action.subject = Some(lf(&cursor.string()?)?);
            }
            "from" if kept.from.is_none() => kept.from = Some(lf(&cursor.string()?)?),
            "addresses" if kept.addresses.is_none() => {
                let addresses = cursor.strings()?;
                kept.addresses = Some(addresses.iter().map(|a| lf(a)).collect::<Option<_>>()?);
            }
            "handle" if kept.handle.is_none() => kept.handle = Some(lf(&cursor.string()?)?),
            // `:mime` makes the reason a MIME entity, which the model does not read.
            _ => return None,
        }
    }
    action.text = lf(&cursor.string()?)?;
    cursor.done().then_some(action)
}

/// `value` with its CRLF line ends as LF, as a host's text holds them; `None` where it holds what
/// no string the writer writes can (a NUL, a CR outside a line end), which the reader therefore
/// does not read as text either.
fn lf(value: &str) -> Option<String> {
    let lf = value.replace("\r\n", "\n");
    (!lf.contains(['\r', '\0'])).then_some(lf)
}
