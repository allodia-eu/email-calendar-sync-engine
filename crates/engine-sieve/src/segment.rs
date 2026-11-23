//! The segmenter: a script split at its top-level commands, byte for byte.
//!
//! Each [`Segment`] is one top-level command with the white space and comments before it, and
//! the rest of the line it ends on: a `# rule:[name]` comment travels with the `if` under it,
//! and the line end after a closing `}` with the `}`. An `if` and the `elsif`/`else` that follow
//! it are one segment, because they are one control structure (RFC 5228 §3.1). Whatever trails
//! the last command is a [`SegmentKind::Trailing`] segment of its own.
//!
//! The segments' texts, in order, are the script: nothing is dropped, added or normalized. That
//! is the whole point. A writer replaces the segments it changes and copies every other one, so
//! a hand-written script survives an edit made elsewhere, comments and layout included.

use core::fmt;

use crate::{
    SieveError, SieveErrorKind,
    lex::{Token, TokenKind, tokenize},
};

/// What a segment holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SegmentKind {
    /// A `require` command (RFC 5228 §3.2).
    Require,
    /// Any other top-level command: an `if` chain, a `keep`, a `set`, an `include`, ….
    Command,
    /// White space and comments after the last command, or a script with no command at all.
    Trailing,
}

/// One top-level command of a script, with the trivia that travels with it.
#[derive(Clone, PartialEq, Eq)]
pub struct Segment<'a> {
    text: &'a str,
    start: usize,
    command_at: usize,
    command_len: usize,
    kind: SegmentKind,
    tokens: Vec<Token<'a>>,
}

impl<'a> Segment<'a> {
    /// The segment's bytes, exactly as in the script.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Where the segment starts, in bytes from the start of the script.
    pub fn start(&self) -> usize {
        self.start
    }

    /// What the segment holds.
    pub fn kind(&self) -> SegmentKind {
        self.kind
    }

    /// The white space and comments before the command: where a dialect keeps a rule's name.
    pub fn leading(&self) -> &'a str {
        &self.text[..self.command_at]
    }

    /// The command itself, from its name through its `;` or closing `}`.
    pub fn command(&self) -> &'a str {
        &self.text[self.command_at..self.command_at + self.command_len]
    }

    /// What follows the command on its line: white space, a hash comment, the line end.
    pub fn trailer(&self) -> &'a str {
        &self.text[self.command_at + self.command_len..]
    }

    /// The command's tokens, trivia inside it included; empty for a trailing segment.
    pub fn tokens(&self) -> &[Token<'a>] {
        &self.tokens
    }

    /// The command's name as written (`if`, `require`, `keep`), or `None` for a trailing segment.
    pub fn identifier(&self) -> Option<&'a str> {
        self.tokens.first().map(Token::text)
    }
}

/// A segment's kind and span; never its text, which may be content.
impl fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Segment")
            .field("kind", &self.kind)
            .field("start", &self.start)
            .field("len", &self.text.len())
            .finish_non_exhaustive()
    }
}

/// Splits `script` at its top-level commands.
///
/// # Errors
///
/// A [`SieveError`] where the script cannot be tokenized, where a top-level token is not a
/// command name, where brackets do not balance, or where it ends inside a command. A script
/// that cannot be split cannot be edited a rule at a time; a caller keeps it whole.
pub fn segment(script: &str) -> Result<Vec<Segment<'_>>, SieveError> {
    let tokens = tokenize(script)?;
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(first) = next_significant(&tokens, i) {
        let command = &tokens[first];
        if command.kind() != TokenKind::Identifier {
            return Err(SieveError::new(
                SieveErrorKind::ExpectedCommand,
                command.start(),
            ));
        }
        let last = command_end(&tokens, first)?;
        let end = line_rest(script, tokens[last].end());
        let kind = if command.is_identifier("require") {
            SegmentKind::Require
        } else {
            SegmentKind::Command
        };
        segments.push(Segment {
            text: &script[start..end],
            start,
            command_at: command.start() - start,
            command_len: tokens[last].end() - command.start(),
            kind,
            tokens: tokens[first..=last].to_vec(),
        });
        start = end;
        i = last + 1;
    }
    if start < script.len() {
        segments.push(Segment {
            text: &script[start..],
            start,
            command_at: script.len() - start,
            command_len: 0,
            kind: SegmentKind::Trailing,
            tokens: Vec::new(),
        });
    }
    Ok(segments)
}

/// Splits `script`, which must be UTF-8, at its top-level commands ([`segment`]).
///
/// # Errors
///
/// [`SieveErrorKind::NotUtf8`] at the first byte that is not UTF-8 (RFC 5228 §2.1 permits other
/// octets in strings and calls them deprecated; such a script is kept whole), or what
/// [`segment`] answers.
pub fn segment_bytes(script: &[u8]) -> Result<Vec<Segment<'_>>, SieveError> {
    let text = core::str::from_utf8(script)
        .map_err(|err| SieveError::new(SieveErrorKind::NotUtf8, err.valid_up_to()))?;
    segment(text)
}

/// The index of the first token at or after `from` that is not trivia.
fn next_significant(tokens: &[Token<'_>], from: usize) -> Option<usize> {
    (from..tokens.len()).find(|&i| !tokens[i].kind().is_trivia())
}

/// The index of the token that ends the command starting at `first`: its `;` outside every
/// bracket, or the closing `}` of its block when no `elsif` or `else` follows.
fn command_end(tokens: &[Token<'_>], first: usize) -> Result<usize, SieveError> {
    let mut open: Vec<TokenKind> = Vec::new();
    let mut i = first + 1;
    while let Some(token) = tokens.get(i) {
        match token.kind() {
            TokenKind::LeftBracket | TokenKind::LeftParen | TokenKind::LeftBrace => {
                open.push(token.kind());
            }
            TokenKind::RightBracket | TokenKind::RightParen | TokenKind::RightBrace => {
                let opener = match token.kind() {
                    TokenKind::RightBracket => TokenKind::LeftBracket,
                    TokenKind::RightParen => TokenKind::LeftParen,
                    _ => TokenKind::LeftBrace,
                };
                if open.pop() != Some(opener) {
                    return Err(SieveError::new(
                        SieveErrorKind::UnbalancedBracket,
                        token.start(),
                    ));
                }
                if token.kind() == TokenKind::RightBrace && open.is_empty() {
                    match next_significant(tokens, i + 1) {
                        Some(next)
                            if tokens[next].is_identifier("elsif")
                                || tokens[next].is_identifier("else") =>
                        {
                            i = next;
                        }
                        _ => return Ok(i),
                    }
                }
            }
            TokenKind::Semicolon if open.is_empty() => return Ok(i),
            _ => {}
        }
        i += 1;
    }
    Err(SieveError::new(
        SieveErrorKind::UnterminatedCommand,
        tokens[first].start(),
    ))
}

/// Where the rest of the line after `end` stops: past spaces and tabs, a hash comment, and one
/// line end, so a command keeps what it shares its line with.
fn line_rest(script: &str, end: usize) -> usize {
    let bytes = script.as_bytes();
    let mut at = end;
    while matches!(bytes.get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    if bytes.get(at) == Some(&b'#') {
        while !matches!(bytes.get(at), None | Some(b'\n')) {
            at += 1;
        }
        if at < bytes.len() {
            return at + 1;
        }
        return at;
    }
    match bytes.get(at..) {
        Some([b'\r', b'\n', ..]) => at + 2,
        Some([b'\n', ..]) => at + 1,
        // Nothing but blanks before the next command on this line: they lead the next one.
        _ => end,
    }
}

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
