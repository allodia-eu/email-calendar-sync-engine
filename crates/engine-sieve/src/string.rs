//! A string token's value, and a value written as a string.

use crate::lex::{Token, TokenKind};

/// The value a string token stands for, or `None` for a token that is no string.
///
/// A quoted string drops its quotes, and a backslash before any character stands for that
/// character (RFC 5228 §2.4.2: `\\` and `\"` are the defined escapes, and an undefined one is
/// read as if the backslash were not there). A `text:` block is its lines after the `text:`
/// line, through the line end before the closing `.` line, which RFC 5228 §2.4.2 makes part of
/// the value ("The CRLF before the final period is considered part of the value"), with one
/// leading `.` removed from a line that starts with two ("dot-stuffing"). So a block's value is
/// empty or ends in a line end. Line ends are kept as written.
///
/// Some writers mean a block's value without that last line end (Roundcube's managesieve
/// plugin strips it on read and adds it on write); a reader of such a dialect strips it itself.
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
    // The last line is the closing `.`; the line end before it is the value's last.
    lines.pop();
    let mut out = String::with_capacity(body.len());
    for line in lines {
        let unstuffed = line.strip_prefix('.').filter(|rest| rest.starts_with('.'));
        out.push_str(unstuffed.unwrap_or(line));
    }
    out
}

/// Whether `value` holds a CR or an LF outside a CRLF pair, or a NUL: what no Sieve string
/// may hold (RFC 5228 §8.1, "CR and LF can only appear in CRLF pairs"; §2.4.2, no NUL).
fn has_bare_line_break_or_nul(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.iter().enumerate().any(|(i, byte)| match byte {
        b'\0' => true,
        b'\r' => bytes.get(i + 1) != Some(&b'\n'),
        b'\n' => i == 0 || bytes[i - 1] != b'\r',
        _ => false,
    })
}

/// `value` as a quoted string, with `\\` and `"` escaped, or `None` where it holds what no Sieve
/// string may: a CR or LF outside a CRLF pair, or a NUL. A CRLF is written as it is; a quoted
/// string may span lines (RFC 5228 §2.4.2).
pub fn quoted(value: &str) -> Option<String> {
    if has_bare_line_break_or_nul(value) {
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

/// `value` as a `text:` block that reads back as exactly `value`: `text:` and CRLF, its lines
/// with a `.` doubled at the start of a line, then a line holding only `.` and its CRLF.
///
/// `None` where no block can hold it: a value that is neither empty nor ends in CRLF (the line
/// end before the closing `.` is the value's own), or one with a CR or LF outside a CRLF pair,
/// or a NUL.
pub fn multi_line(value: &str) -> Option<String> {
    if has_bare_line_break_or_nul(value) || !(value.is_empty() || value.ends_with("\r\n")) {
        return None;
    }
    let mut out = String::with_capacity(value.len() + 12);
    out.push_str("text:\r\n");
    for line in value.split_inclusive("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }
    out.push_str(".\r\n");
    Some(out)
}

#[cfg(test)]
#[path = "string_tests.rs"]
mod tests;
