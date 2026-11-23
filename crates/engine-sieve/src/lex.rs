//! The lexer: a Sieve script's tokens (RFC 5228 §8.1), each with the bytes it spans.
//!
//! It knows exactly what it takes to find where a command ends: strings (quoted and `text:`),
//! comments (hash and bracket), brackets, and the punctuation between them. Nothing is decoded
//! here; a token is a span of the script, and the spans of all tokens, in order, are the whole
//! script. [`crate::string`] decodes a string token's value when a reader wants it.
//!
//! Real scripts are not always CRLF, as RFC 5228 §2.2 asks: a script stored from a Unix editor
//! uses LF. Both are read as line ends, and neither is rewritten.

use core::fmt;

use crate::{SieveError, SieveErrorKind};

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TokenKind {
    /// A command or test name, or `true`/`false`: `[A-Za-z_][A-Za-z0-9_]*`.
    Identifier,
    /// A tagged argument, `:` then an identifier (`:contains`).
    Tag,
    /// Digits, with an optional `K`, `M` or `G` quantifier.
    Number,
    /// A `"…"` string, quotes included.
    QuotedString,
    /// A `text:` block, through the line holding only `.`.
    MultiLine,
    /// `[`
    LeftBracket,
    /// `]`
    RightBracket,
    /// `(`
    LeftParen,
    /// `)`
    RightParen,
    /// `{`
    LeftBrace,
    /// `}`
    RightBrace,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// Spaces, tabs and line ends.
    Whitespace,
    /// `#` to the end of its line, the line end not included.
    HashComment,
    /// `/* … */`.
    BracketComment,
}

impl TokenKind {
    /// Whether the token separates others and means nothing itself: white space or a comment.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::HashComment | Self::BracketComment
        )
    }

    /// Whether the token is a string, quoted or `text:`.
    pub fn is_string(self) -> bool {
        matches!(self, Self::QuotedString | Self::MultiLine)
    }
}

/// One token: its kind and the bytes of the script it spans.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
    start: usize,
}

impl<'a> Token<'a> {
    /// What the token is.
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// The token's bytes, exactly as in the script.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Where the token starts, in bytes from the start of the script.
    pub fn start(&self) -> usize {
        self.start
    }

    /// Where the token ends: the offset of the byte after it.
    pub fn end(&self) -> usize {
        self.start + self.text.len()
    }

    /// Whether this is the identifier `name`, compared as Sieve compares identifiers: ignoring
    /// ASCII case (RFC 5228 §8.1).
    pub fn is_identifier(&self, name: &str) -> bool {
        self.kind == TokenKind::Identifier && self.text.eq_ignore_ascii_case(name)
    }

    /// Whether this is the tag `:name`, ignoring ASCII case.
    pub fn is_tag(&self, name: &str) -> bool {
        self.kind == TokenKind::Tag && self.text[1..].eq_ignore_ascii_case(name)
    }
}

/// A token's kind and span; never its text, which may be content.
impl fmt::Debug for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("kind", &self.kind)
            .field("start", &self.start)
            .field("len", &self.text.len())
            .finish()
    }
}

/// Splits `script` into its tokens, trivia included, so that their texts in order are the
/// whole script.
///
/// # Errors
///
/// A [`SieveError`] at the first byte that starts no token, or at the start of a string or
/// comment that is not closed.
pub fn tokenize(script: &str) -> Result<Vec<Token<'_>>, SieveError> {
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < script.len() {
        let (kind, end) = next(script, at)?;
        tokens.push(Token {
            kind,
            text: &script[at..end],
            start: at,
        });
        at = end;
    }
    Ok(tokens)
}

/// The kind and end of the token starting at `at`.
fn next(script: &str, at: usize) -> Result<(TokenKind, usize), SieveError> {
    let bytes = script.as_bytes();
    let rest = &bytes[at..];
    let single = |kind| Ok((kind, at + 1));
    match rest[0] {
        b' ' | b'\t' | b'\r' | b'\n' => Ok((
            TokenKind::Whitespace,
            at + count(rest, |b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')),
        )),
        b'#' => Ok((TokenKind::HashComment, line_end(bytes, at))),
        b'/' if rest.get(1) == Some(&b'*') => find(bytes, at + 2, b"*/")
            .map(|end| (TokenKind::BracketComment, end + 2))
            .ok_or(SieveError::new(SieveErrorKind::UnterminatedComment, at)),
        b'"' => quoted(bytes, at).map(|end| (TokenKind::QuotedString, end)),
        b':' if rest.get(1).is_some_and(|b| is_identifier_start(*b)) => Ok((
            TokenKind::Tag,
            at + 1 + count(&rest[1..], is_identifier_byte),
        )),
        b if is_identifier_start(b) => {
            let end = at + count(rest, is_identifier_byte);
            if script[at..end].eq_ignore_ascii_case("text") && bytes.get(end) == Some(&b':') {
                multi_line(bytes, at, end + 1).map(|end| (TokenKind::MultiLine, end))
            } else {
                Ok((TokenKind::Identifier, end))
            }
        }
        b'0'..=b'9' => {
            let digits = at + count(rest, |b| b.is_ascii_digit());
            let quantified = bytes
                .get(digits)
                .is_some_and(|b| matches!(b.to_ascii_uppercase(), b'K' | b'M' | b'G'));
            Ok((TokenKind::Number, digits + usize::from(quantified)))
        }
        b'[' => single(TokenKind::LeftBracket),
        b']' => single(TokenKind::RightBracket),
        b'(' => single(TokenKind::LeftParen),
        b')' => single(TokenKind::RightParen),
        b'{' => single(TokenKind::LeftBrace),
        b'}' => single(TokenKind::RightBrace),
        b',' => single(TokenKind::Comma),
        b';' => single(TokenKind::Semicolon),
        _ => Err(SieveError::new(SieveErrorKind::UnexpectedCharacter, at)),
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// How many leading bytes of `bytes` satisfy `keep`.
fn count(bytes: &[u8], keep: impl Fn(u8) -> bool) -> usize {
    bytes.iter().take_while(|b| keep(**b)).count()
}

/// The offset of the line end (`\r\n` or `\n`) at or after `at`, or the script's end.
fn line_end(bytes: &[u8], at: usize) -> usize {
    let mut end = at + count(&bytes[at..], |b| b != b'\n');
    if end > at && bytes[end - 1] == b'\r' && end < bytes.len() {
        end -= 1;
    }
    end
}

/// The offset of the first `needle` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|found| from + found)
}

/// The end of the quoted string opening at `at`: after its closing quote. A backslash escapes
/// whatever follows it (RFC 5228 §2.4.2), a line end included.
fn quoted(bytes: &[u8], at: usize) -> Result<usize, SieveError> {
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(SieveError::new(SieveErrorKind::UnterminatedString, at))
}

/// The end of the `text:` block whose `text:` ends at `after_colon`: after the line holding
/// only `.` (RFC 5228 §8.1). What may follow `text:` on its own line is white space and a hash
/// comment.
fn multi_line(bytes: &[u8], at: usize, after_colon: usize) -> Result<usize, SieveError> {
    let unterminated = || SieveError::new(SieveErrorKind::UnterminatedMultiLine, at);
    let mut i = after_colon + count(&bytes[after_colon..], |b| matches!(b, b' ' | b'\t'));
    if bytes.get(i) == Some(&b'#') {
        i = line_end(bytes, i);
    }
    // The first line of the block starts after the line end that closes the `text:` line.
    i = next_line(bytes, i).ok_or_else(unterminated)?;
    loop {
        let end = line_end(bytes, i);
        if &bytes[i..end] == b"." {
            return Ok(next_line(bytes, end).unwrap_or(end));
        }
        i = next_line(bytes, end).ok_or_else(unterminated)?;
    }
}

/// The start of the line after the line end at `at`, or `None` where no line end is there.
fn next_line(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at..)? {
        [b'\r', b'\n', ..] => Some(at + 2),
        [b'\n', ..] => Some(at + 1),
        _ => None,
    }
}

#[cfg(test)]
#[path = "lex_tests.rs"]
mod tests;
