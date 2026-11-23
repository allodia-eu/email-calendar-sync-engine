//! Why a script could not be read.

use core::fmt;

/// A script the lexer or the segmenter could not read, and where.
///
/// It names a class and a byte offset, never the script's text: rule values are content
/// (`north-star.md`, security).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SieveError {
    kind: SieveErrorKind,
    offset: usize,
}

/// What was wrong with the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SieveErrorKind {
    /// A quoted string with no closing `"`.
    UnterminatedString,
    /// A `text:` block with no line holding only `.`.
    UnterminatedMultiLine,
    /// A `/*` comment with no `*/`.
    UnterminatedComment,
    /// A byte that starts no token (RFC 5228 §8.1).
    UnexpectedCharacter,
    /// A closing bracket that does not match the one it closes, or one with nothing open.
    UnbalancedBracket,
    /// A token that cannot begin a command where a command must begin (RFC 5228 §8.2: a
    /// command starts with an identifier).
    ExpectedCommand,
    /// The script ended inside a command: no `;` or closing `}` came.
    UnterminatedCommand,
    /// A `require` whose argument is not a string or a string list (RFC 5228 §3.2).
    MalformedRequire,
    /// The script is not UTF-8 (RFC 5228 §2.1).
    NotUtf8,
    /// A value to be written that no Sieve string can hold: a CR or LF outside a CRLF pair, or a
    /// NUL. It was never in a script, so its offset is 0.
    UnwritableValue,
}

impl SieveError {
    pub(crate) fn new(kind: SieveErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }

    /// What was wrong.
    pub fn kind(&self) -> SieveErrorKind {
        self.kind
    }

    /// The byte offset in the script where it was found.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for SieveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            SieveErrorKind::UnterminatedString => "a quoted string is not closed",
            SieveErrorKind::UnterminatedMultiLine => "a text: block is not closed",
            SieveErrorKind::UnterminatedComment => "a bracket comment is not closed",
            SieveErrorKind::UnexpectedCharacter => "a character starts no token",
            SieveErrorKind::UnbalancedBracket => "a bracket is not balanced",
            SieveErrorKind::ExpectedCommand => "a command does not start with an identifier",
            SieveErrorKind::UnterminatedCommand => "the script ends inside a command",
            SieveErrorKind::MalformedRequire => "a require names no string or string list",
            SieveErrorKind::NotUtf8 => "the script is not UTF-8",
            SieveErrorKind::UnwritableValue => "a value cannot be written as a Sieve string",
        };
        write!(f, "Sieve script: {what} (at byte {})", self.offset)
    }
}

impl std::error::Error for SieveError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_says_what_and_where_and_nothing_else() {
        let kinds = [
            SieveErrorKind::UnterminatedString,
            SieveErrorKind::UnterminatedMultiLine,
            SieveErrorKind::UnterminatedComment,
            SieveErrorKind::UnexpectedCharacter,
            SieveErrorKind::UnbalancedBracket,
            SieveErrorKind::ExpectedCommand,
            SieveErrorKind::UnterminatedCommand,
            SieveErrorKind::MalformedRequire,
            SieveErrorKind::NotUtf8,
            SieveErrorKind::UnwritableValue,
        ];
        let texts: std::collections::BTreeSet<String> = kinds
            .iter()
            .map(|kind| SieveError::new(*kind, 42).to_string())
            .collect();
        assert_eq!(texts.len(), kinds.len(), "each kind reads differently");
        assert!(texts.iter().all(|text| text.ends_with("(at byte 42)")));
    }
}
