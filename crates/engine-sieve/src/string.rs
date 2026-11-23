//! A string token's value, and a value written as a string.

use crate::lex::{Token, TokenKind};

/// The value a string token stands for, or `None` for a token that is no string.
///
/// A quoted string drops its quotes, and a backslash before any character stands for that
/// character (RFC 5228 §2.4.2: `\\` and `\"` are the defined escapes, and an undefined one is
/// read as if the backslash were not there). A `text:` block is its lines after the `text:`
/// line, without the closing `.` line or the line end before it, with one leading `.` removed
/// from a line that starts with two (RFC 5228 §2.4.2, "dot-stuffing"). Line ends are kept as
/// written.
///
/// Neither form expands `encoded-character` (RFC 5228 §2.4.2.4) or variables (RFC 5229): a
/// reader that finds `${` in a value where either extension is required must not read the value
/// as literal text.
pub fn value(token: &Token<'_>) -> Option<String> {
    match token.kind() {
        TokenKind::QuotedString => {
            let text = token.text();
            Some(unescape(&text[1..text.len() - 1]))
        }
        TokenKind::MultiLine => Some(multi_line_value(token.text())),
        _ => None,
    }
}

fn unescape(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn multi_line_value(text: &str) -> String {
    // The block's lines start after the first line end; the `text:` line itself carries only
    // white space or a comment.
    let body = text.split_once('\n').map_or("", |(_, body)| body);
    let mut lines: Vec<&str> = body.split_inclusive('\n').collect();
    // The last line is the closing `.`; the line end before it belongs to the syntax.
    lines.pop();
    let mut out = String::with_capacity(body.len());
    for line in lines {
        let unstuffed = line.strip_prefix('.').filter(|rest| rest.starts_with('.'));
        out.push_str(unstuffed.unwrap_or(line));
    }
    if out.ends_with("\r\n") {
        out.truncate(out.len() - 2);
    } else if out.ends_with('\n') {
        out.truncate(out.len() - 1);
    }
    out
}

/// `value` as a quoted string, with `\` and `"` escaped, or `None` where it holds a character
/// a quoted string should not: a CR, an LF or a NUL. A caller writes such a value as a `text:`
/// block ([`multi_line`]), or refuses it.
pub fn quoted(value: &str) -> Option<String> {
    if value.contains(['\r', '\n', '\0']) {
        return None;
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// `value` as a `text:` block: `text:`, CRLF, its lines with CRLF line ends and a `.` doubled at
/// the start of a line, then a line holding only `.` and its CRLF. `None` where it holds a NUL,
/// which no Sieve string may (RFC 5228 §2.4.2).
pub fn multi_line(value: &str) -> Option<String> {
    if value.contains('\0') {
        return None;
    }
    let mut out = String::from("text:\r\n");
    for line in value.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    Some(out)
}

#[cfg(test)]
#[path = "string_tests.rs"]
mod tests;
