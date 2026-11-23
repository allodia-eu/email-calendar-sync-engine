//! The out-of-office reply rule, read: Roundcube's vacation engine's shape, and nothing wider.
//!
//! The rule is `if <window>` then a block whose one command is `vacation`. The window is `true`,
//! one `currentdate :zone … :value … …` test, or `allof` of two, and the rule may be disabled
//! (`if false # <window>`). Anything else in it would change who is answered or what else
//! happens to the message (another test, another action, a `:mime` body, a `currentdate` in the
//! server's own zone), so a rule holding it is the reply, unread ([`Shape::Unreadable`]).

use engine_core::time::UtcDateTime;

use super::cursor::Cursor;
use crate::lex::{Token, TokenKind, tokenize};

/// A day, in seconds.
const DAY: u64 = 24 * 60 * 60;

/// What a reply rule says, as far as the model reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ReadReply {
    pub(super) enabled: bool,
    pub(super) start: Option<UtcDateTime>,
    pub(super) end: Option<UtcDateTime>,
    pub(super) subject: Option<String>,
    /// The reason, its line ends as LF.
    pub(super) text: String,
    /// The arguments the model does not carry, kept for a rewrite.
    pub(super) kept: Kept,
}

/// `vacation`'s arguments the model leaves to the server or the script (decision 6 of the design
/// note: the interval is the server's), written back as they were read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) days: Option<u64>,
    pub(super) seconds: Option<u64>,
    pub(super) from: Option<String>,
    pub(super) addresses: Option<Vec<String>>,
    pub(super) handle: Option<String>,
}

/// Whether a segment's command is the reply, and what it says.
pub(super) enum Shape {
    /// Not a reply rule.
    Other,
    /// The reply, read.
    Read(ReadReply),
    /// The reply, in a shape this reader does not read.
    Unreadable,
}

/// What the command in `tokens` (one top-level segment's) is: Roundcube takes as the reply the
/// first rule whose first action is `vacation`; a bare top-level `vacation` is a reply too, unread.
pub(super) fn shape(tokens: &[Token<'_>]) -> Shape {
    let significant: Vec<&Token<'_>> = tokens
        .iter()
        .filter(|token| token.kind() != TokenKind::Whitespace)
        .collect();
    let Some(first) = significant.first() else {
        return Shape::Other;
    };
    if first.is_identifier("vacation") {
        return Shape::Unreadable;
    }
    if !first.is_identifier("if") {
        return Shape::Other;
    }
    let Some(brace) = block_start(&significant) else {
        return Shape::Other;
    };
    let in_block = significant[brace + 1..]
        .iter()
        .find(|token| !token.kind().is_trivia());
    if !in_block.is_some_and(|token| token.is_identifier("vacation")) {
        return Shape::Other;
    }
    read(&significant, brace).map_or(Shape::Unreadable, Shape::Read)
}

/// Where the `if`'s block opens: the first `{` outside a comment.
fn block_start(significant: &[&Token<'_>]) -> Option<usize> {
    significant
        .iter()
        .position(|token| token.kind() == TokenKind::LeftBrace)
}

/// The reply rule read, or `None` where it holds what the model does not.
fn read(significant: &[&Token<'_>], brace: usize) -> Option<ReadReply> {
    let test = &significant[1..brace];
    let (enabled, window) = match test {
        [word, comment]
            if word.is_identifier("false") && comment.kind() == TokenKind::HashComment =>
        {
            let commented = comment.text()[1..].trim_start_matches(' ');
            let tokens = tokenize(commented).ok()?;
            let mut cursor = Cursor::new(&tokens, true);
            let window = window(&mut cursor)?;
            (false, window)
        }
        _ => {
            let mut cursor = Cursor::new(test.iter().copied(), true);
            (true, window(&mut cursor)?)
        }
    };
    let mut cursor = Cursor::new(significant[brace + 1..].iter().copied(), true);
    cursor.next()?; // `vacation`
    let mut reply = action(&mut cursor)?;
    cursor.eat(TokenKind::Semicolon)?;
    cursor.eat(TokenKind::RightBrace)?;
    if !cursor.done() {
        return None;
    }
    reply.enabled = enabled;
    (reply.start, reply.end) = window;
    Some(reply)
}

type Window = (Option<UtcDateTime>, Option<UtcDateTime>);

/// `true`, one `currentdate` test, or `allof` of tests each bounding one end once.
fn window(cursor: &mut Cursor<'_, '_>) -> Option<Window> {
    let token = cursor.next()?;
    let mut window = (None, None);
    if token.is_identifier("allof") {
        cursor.eat(TokenKind::LeftParen)?;
        loop {
            bound(cursor, &mut window)?;
            if cursor.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        cursor.eat(TokenKind::RightParen)?;
    } else if token.is_identifier("currentdate") {
        bound_test(cursor, &mut window)?;
    } else if !token.is_identifier("true") {
        return None;
    }
    cursor.done().then_some(window)
}

fn bound(cursor: &mut Cursor<'_, '_>, window: &mut Window) -> Option<()> {
    cursor.next()?.is_identifier("currentdate").then_some(())?;
    bound_test(cursor, window)
}

/// The rest of `currentdate :zone "<±hhmm>" :value "<rel>" "<part>" "<value>"`, tags in either
/// order, setting the end it bounds. Without `:zone` the date is the server's local one, which the
/// reader cannot know, so it does not read it.
fn bound_test(cursor: &mut Cursor<'_, '_>, window: &mut Window) -> Option<()> {
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
    // The first whole second the test holds at, or the first it no longer does.
    let (lower, at) = match part.as_str() {
        "iso8601" => {
            let at = UtcDateTime::parse_rfc3339(&value).ok()?;
            // The current date is rendered in `zone`, and compared with the value as text, so the
            // value must be written in the same zone to compare as an instant.
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

/// The offset an RFC 3339 value is written in, in seconds east of UTC.
fn value_offset(value: &str) -> Option<i64> {
    if value.ends_with('Z') || value.ends_with('z') {
        return Some(0);
    }
    let (body, sign) = match value
        .len()
        .checked_sub(6)
        .map(|at| (at, value.as_bytes()[at]))
    {
        Some((at, b'+')) => (&value[at + 1..], 1),
        Some((at, b'-')) => (&value[at + 1..], -1),
        _ => return None,
    };
    let (hours, minutes) = body.split_once(':')?;
    Some(sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60))
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
fn action(cursor: &mut Cursor<'_, '_>) -> Option<ReadReply> {
    let mut reply = ReadReply::default();
    let kept = &mut reply.kept;
    while let Some(tag) = cursor.tag() {
        match tag.as_str() {
            "days" if kept.days.is_none() => kept.days = Some(cursor.number()?),
            "seconds" if kept.seconds.is_none() => kept.seconds = Some(cursor.number()?),
            "subject" if reply.subject.is_none() => reply.subject = Some(lf(&cursor.string()?)),
            "from" if kept.from.is_none() => kept.from = Some(cursor.string()?),
            "addresses" if kept.addresses.is_none() => kept.addresses = Some(cursor.strings()?),
            "handle" if kept.handle.is_none() => kept.handle = Some(cursor.string()?),
            // `:mime` makes the reason a MIME entity, which the model does not read.
            _ => return None,
        }
    }
    reply.text = lf(&cursor.string()?);
    Some(reply)
}

/// `value` with its CRLF line ends as LF, as a host's text holds them.
fn lf(value: &str) -> String {
    value.replace("\r\n", "\n")
}
