//! A cursor over a command's significant tokens, reading the arguments the dialect uses.

use crate::{
    lex::{Token, TokenKind},
    string,
};

/// The significant tokens of one command (or of one test), and how far they have been read.
pub(super) struct Cursor<'t, 'a> {
    tokens: Vec<&'t Token<'a>>,
    at: usize,
    /// Whether a value may be read as literal text: not where the script requires an extension
    /// that expands `${…}` in strings (`variables`, `encoded-character`) and the value has one.
    literal: bool,
}

impl<'t, 'a> Cursor<'t, 'a> {
    pub(super) fn new(tokens: impl IntoIterator<Item = &'t Token<'a>>, literal: bool) -> Self {
        Self {
            tokens: tokens
                .into_iter()
                .filter(|token| !token.kind().is_trivia())
                .collect(),
            at: 0,
            literal,
        }
    }

    pub(super) fn peek(&self) -> Option<&'t Token<'a>> {
        self.tokens.get(self.at).copied()
    }

    pub(super) fn next(&mut self) -> Option<&'t Token<'a>> {
        let token = self.peek()?;
        self.at += 1;
        Some(token)
    }

    pub(super) fn done(&self) -> bool {
        self.at == self.tokens.len()
    }

    /// Consumes a token of `kind`, or answers `None` and consumes nothing.
    pub(super) fn eat(&mut self, kind: TokenKind) -> Option<()> {
        (self.peek()?.kind() == kind).then(|| self.at += 1)
    }

    /// The next token's name, lowercased, if it is a tag; consumes it.
    pub(super) fn tag(&mut self) -> Option<String> {
        let token = self.peek()?;
        (token.kind() == TokenKind::Tag).then(|| {
            self.at += 1;
            token.text()[1..].to_ascii_lowercase()
        })
    }

    /// One string's value. A `text:` block's value is read without its last line end, as the
    /// dialect writes one (`string::value` keeps it, RFC 5228 §2.4.2).
    pub(super) fn string(&mut self) -> Option<String> {
        let token = self.peek()?;
        let mut value = string::value(token)?;
        if token.kind() == TokenKind::MultiLine {
            if value.ends_with("\r\n") {
                value.truncate(value.len() - 2);
            } else if value.ends_with('\n') {
                value.truncate(value.len() - 1);
            }
        }
        if !self.literal && value.contains("${") {
            return None;
        }
        self.at += 1;
        Some(value)
    }

    /// A string, or a bracketed list of at least one.
    pub(super) fn strings(&mut self) -> Option<Vec<String>> {
        if self.eat(TokenKind::LeftBracket).is_none() {
            return self.string().map(|value| vec![value]);
        }
        let mut values = vec![self.string()?];
        while self.eat(TokenKind::Comma).is_some() {
            values.push(self.string()?);
        }
        self.eat(TokenKind::RightBracket)?;
        Some(values)
    }

    /// A number with its quantifier applied (RFC 5228 §2.4.1: K, M and G are powers of 1024).
    pub(super) fn number(&mut self) -> Option<u64> {
        let token = self.peek()?;
        if token.kind() != TokenKind::Number {
            return None;
        }
        let text = token.text();
        let (digits, unit) = match text.as_bytes()[text.len() - 1].to_ascii_uppercase() {
            b'K' => (&text[..text.len() - 1], 1u64 << 10),
            b'M' => (&text[..text.len() - 1], 1 << 20),
            b'G' => (&text[..text.len() - 1], 1 << 30),
            _ => (text, 1),
        };
        let value = digits.parse::<u64>().ok()?.checked_mul(unit)?;
        self.at += 1;
        Some(value)
    }
}
