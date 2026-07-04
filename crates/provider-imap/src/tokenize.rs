//! The IMAP data-grammar tokenizer (RFC 9051 §4).
//!
//! The shared primitive behind every response parser in [`crate::parse`]: a
//! recursive-descent tokenizer over one assembled response body (the bytes after
//! the leading `* `, with any `{n}` literals already inlined — see
//! [`crate::transport`]). It reduces a body to an [`Item`] tree which the `parse_*`
//! readers interpret. Everything here is pure and offline-tested against
//! adversarial input: a malformed body is an [`ImapError::Protocol`], **never** a
//! panic (`north-star.md` security).

use crate::error::{ImapError, ImapResult};

/// Maximum list nesting accepted, so adversarial input (`((((((…`) is rejected
/// rather than overflowing the stack — hostile mail must never crash the parser
/// (`north-star.md` security). Real responses nest only a few levels (`ENVELOPE`'s
/// address lists).
const MAX_DEPTH: usize = 64;

/// A parsed IMAP data item — the recursive shape every response body reduces to
/// (RFC 9051 §4). Interpreted into domain structs by the `parse_*` functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    /// `NIL` — an absent value.
    Nil,
    /// An unquoted atom: a number, a flag (`\Seen`), a keyword, or `FETCH`.
    Atom(String),
    /// A quoted string, with `\"`/`\\` escapes resolved.
    Quoted(String),
    /// A `{n}` literal's raw bytes.
    Literal(Vec<u8>),
    /// A parenthesized list of items.
    List(Vec<Item>),
}

impl Item {
    /// The item as a UTF-8 string when it is a quoted string or literal; `None`
    /// for `NIL`. Lossy on non-UTF-8 literal bytes (mail is hostile input).
    pub(crate) fn as_nstring(&self) -> Option<String> {
        match self {
            Self::Quoted(s) => Some(s.clone()),
            Self::Literal(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            // An atom in a string position is unusual but harmless to accept.
            Self::Atom(a) => Some(a.clone()),
            Self::Nil | Self::List(_) => None,
        }
    }

    /// The item as an atom string (a flag, number, or keyword).
    pub(crate) fn as_atom(&self) -> Option<&str> {
        match self {
            Self::Atom(a) => Some(a),
            _ => None,
        }
    }

    /// The item as a parenthesized list.
    pub(crate) fn as_list(&self) -> Option<&[Item]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

/// A recursive-descent tokenizer over one assembled response body.
struct Tokens<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Tokens<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(b' ')) {
            self.pos += 1;
        }
    }

    /// Parses every space-separated item to end of buffer (the top level of a
    /// response body).
    fn parse_all(&mut self) -> ImapResult<Vec<Item>> {
        let mut items = Vec::new();
        loop {
            self.skip_spaces();
            // Trailing CR/LF the transport may have left on the logical line.
            while matches!(self.peek(), Some(b'\r' | b'\n')) {
                self.pos += 1;
            }
            if self.peek().is_none() {
                return Ok(items);
            }
            items.push(self.parse_item()?);
        }
    }

    /// Parses one item.
    fn parse_item(&mut self) -> ImapResult<Item> {
        self.skip_spaces();
        match self.peek() {
            Some(b'(') => self.parse_list(),
            Some(b'"') => self.parse_quoted(),
            Some(b'{') => self.parse_literal(),
            Some(_) => self.parse_atom(),
            None => Err(ImapError::protocol("unexpected end of response")),
        }
    }

    /// Parses a parenthesized list, consuming the closing `)`. Nesting is bounded
    /// by [`MAX_DEPTH`] so adversarial input cannot overflow the stack.
    fn parse_list(&mut self) -> ImapResult<Item> {
        self.bump(); // consume '('
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ImapError::protocol("list nested too deeply"));
        }
        let mut items = Vec::new();
        loop {
            self.skip_spaces();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Item::List(items));
                }
                None => return Err(ImapError::protocol("unterminated list")),
                _ => items.push(self.parse_item()?),
            }
        }
    }

    /// Parses a `"quoted string"`, resolving `\"` and `\\` escapes.
    fn parse_quoted(&mut self) -> ImapResult<Item> {
        self.bump(); // consume opening '"'
        // Accumulate raw bytes and decode the whole run as UTF-8 (lossy) at the end.
        // A per-byte `as char` cast would map each byte to a Latin-1 codepoint, so a
        // quoted string carrying raw UTF-8 (a display name or a `UTF8=ACCEPT` mailbox
        // name) would be mojibake — the literal path already decodes correctly, and
        // the two must agree.
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.bump() {
                Some(b'"') => return Ok(Item::Quoted(String::from_utf8_lossy(&out).into_owned())),
                Some(b'\\') => match self.bump() {
                    Some(c @ (b'"' | b'\\')) => out.push(c),
                    _ => return Err(ImapError::protocol("bad escape in quoted string")),
                },
                Some(b'\r' | b'\n') => {
                    return Err(ImapError::protocol("CR/LF in quoted string"));
                }
                Some(c) => out.push(c),
                None => return Err(ImapError::protocol("unterminated quoted string")),
            }
        }
    }

    /// Parses a `{n}` literal: the count, the required CRLF, then exactly `n`
    /// bytes that the transport inlined after it.
    fn parse_literal(&mut self) -> ImapResult<Item> {
        self.bump(); // consume '{'
        let mut digits = String::new();
        loop {
            match self.bump() {
                Some(b'}') => break,
                Some(c @ b'0'..=b'9') => digits.push(c as char),
                _ => return Err(ImapError::protocol("malformed literal length")),
            }
        }
        let n: usize = digits
            .parse()
            .map_err(|_| ImapError::protocol("literal length not a number"))?;
        // The CRLF after `}` — tolerate a bare LF too.
        if self.peek() == Some(b'\r') {
            self.pos += 1;
        }
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| ImapError::protocol("literal longer than response"))?;
        let bytes = self.buf[self.pos..end].to_vec();
        self.pos = end;
        Ok(Item::Literal(bytes))
    }

    /// Parses an atom: a run up to the next space, paren, or end. `NIL` becomes
    /// [`Item::Nil`].
    fn parse_atom(&mut self) -> ImapResult<Item> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'(' | b')' | b'\r' | b'\n') {
                break;
            }
            self.pos += 1;
        }
        // A break character in item position (a stray `)` at top level) consumes
        // nothing; erroring here keeps the caller's loop from spinning forever.
        if self.pos == start {
            return Err(ImapError::protocol("expected an item"));
        }
        let atom = String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned();
        if atom.eq_ignore_ascii_case("NIL") {
            Ok(Item::Nil)
        } else {
            Ok(Item::Atom(atom))
        }
    }
}

/// Parses a response body into its top-level items.
pub(crate) fn items_of(line: &[u8]) -> ImapResult<Vec<Item>> {
    Tokens::new(line).parse_all()
}
