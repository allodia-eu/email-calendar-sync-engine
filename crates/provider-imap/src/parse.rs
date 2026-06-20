//! Pure IMAP response parsing (RFC 9051 §4, §7).
//!
//! The transport hands each untagged response body (the bytes after the leading
//! `* `, with any `{n}` literals already inlined — see [`crate::transport`]) to the
//! `parse_*` functions here, which interpret them into the small structs the
//! normalizer ([`crate::mail`]) maps to the domain model. Everything in this module
//! is pure and offline-tested, including against adversarial input: a malformed
//! response is an [`ImapError::Protocol`], **never** a panic (`north-star.md`
//! security).
//!
//! The shared primitive is a tokenizer over the recursive IMAP data grammar
//! (`NIL` / atom / quoted-string / `{n}` literal / parenthesized list); each
//! response is then read off the resulting [`Item`] tree.

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
    fn as_nstring(&self) -> Option<String> {
        match self {
            Self::Quoted(s) => Some(s.clone()),
            Self::Literal(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            // An atom in a string position is unusual but harmless to accept.
            Self::Atom(a) => Some(a.clone()),
            Self::Nil | Self::List(_) => None,
        }
    }

    /// The item as an atom string (a flag, number, or keyword).
    fn as_atom(&self) -> Option<&str> {
        match self {
            Self::Atom(a) => Some(a),
            _ => None,
        }
    }

    /// The item as a parenthesized list.
    fn as_list(&self) -> Option<&[Item]> {
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
fn items_of(line: &[u8]) -> ImapResult<Vec<Item>> {
    Tokens::new(line).parse_all()
}

/// What a `SELECT`/`EXAMINE` told us about the mailbox: its UID space and message
/// count (RFC 9051 §6.3.2, §7.3.1, §7.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectData {
    /// `UIDVALIDITY` — bumps when the server renumbers the UID space (a reset).
    pub uid_validity: u32,
    /// `UIDNEXT` — the UID the next delivered message will get, if advertised.
    pub uid_next: Option<u32>,
    /// `EXISTS` — the number of messages in the mailbox.
    pub exists: u32,
}

/// One parsed `ENVELOPE` address `(name adl mailbox host)` (RFC 9051 §7.5.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Address {
    /// The display name, if present.
    pub name: Option<String>,
    /// The local part (before `@`).
    pub mailbox: Option<String>,
    /// The domain (after `@`).
    pub host: Option<String>,
}

/// The parsed `ENVELOPE` fields the normalizer reads (RFC 9051 §7.5.2). `References`
/// is not an envelope field; it is left to a later threading slice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Envelope {
    /// The `Subject`.
    pub subject: Option<String>,
    /// The `From` addresses.
    pub from: Vec<Address>,
    /// The `Sender` addresses.
    pub sender: Vec<Address>,
    /// The `Reply-To` addresses.
    pub reply_to: Vec<Address>,
    /// The `To` recipients.
    pub to: Vec<Address>,
    /// The `Cc` recipients.
    pub cc: Vec<Address>,
    /// The `Bcc` recipients.
    pub bcc: Vec<Address>,
    /// The raw `In-Reply-To` value (with angle brackets), if present.
    pub in_reply_to: Option<String>,
    /// The raw `Message-ID` value (with angle brackets), if present.
    pub message_id: Option<String>,
}

/// One row of a `UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchRow {
    /// The mailbox-unique UID (RFC 9051 §2.3.1.1) — the identity component.
    pub uid: u32,
    /// The IMAP flags (`\Seen`, `\Flagged`, custom keywords).
    pub flags: Vec<String>,
    /// The raw `INTERNALDATE` string (`"dd-Mon-yyyy hh:mm:ss +zzzz"`).
    pub internal_date: Option<String>,
    /// `RFC822.SIZE` in octets.
    pub size: Option<u64>,
    /// The parsed `ENVELOPE`, if requested and present.
    pub envelope: Option<Envelope>,
}

/// One `LIST` row: a mailbox's attributes, hierarchy delimiter, and name
/// (RFC 9051 §7.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListRow {
    /// The name attributes (`\HasNoChildren`, `\Sent`, `\Noselect`, …).
    pub attributes: Vec<String>,
    /// The hierarchy delimiter, or `None` for a flat namespace (`NIL`).
    pub delimiter: Option<String>,
    /// The mailbox name/path.
    pub name: String,
}

/// Scans a response line for a bracketed response-code number like
/// `[UIDVALIDITY 12345]` (RFC 9051 §7.1).
fn response_code_number(line: &[u8], code: &str) -> Option<u32> {
    let text = String::from_utf8_lossy(line);
    let needle = format!("[{code} ");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

/// The message count from a `* <n> EXISTS` line, if that is what this line is.
fn exists_count(line: &[u8]) -> Option<u32> {
    let items = items_of(line).ok()?;
    let [first, second, ..] = items.as_slice() else {
        return None;
    };
    if !second
        .as_atom()
        .is_some_and(|a| a.eq_ignore_ascii_case("EXISTS"))
    {
        return None;
    }
    first.as_atom().and_then(|a| a.parse().ok())
}

/// Reads `SELECT`/`EXAMINE` untagged responses into a [`SelectData`]. A missing
/// `UIDVALIDITY` is an [`ImapError::Protocol`]: the sync layer cannot key identity
/// without it.
pub(crate) fn parse_select(lines: &[Vec<u8>]) -> ImapResult<SelectData> {
    let mut uid_validity = None;
    let mut uid_next = None;
    let mut exists = 0;
    for line in lines {
        if let Some(v) = response_code_number(line, "UIDVALIDITY") {
            uid_validity = Some(v);
        }
        if let Some(v) = response_code_number(line, "UIDNEXT") {
            uid_next = Some(v);
        }
        if let Some(n) = exists_count(line) {
            exists = n;
        }
    }
    let uid_validity = uid_validity
        .ok_or_else(|| ImapError::protocol("SELECT response carried no UIDVALIDITY"))?;
    Ok(SelectData {
        uid_validity,
        uid_next,
        exists,
    })
}

/// Reads `LIST` untagged responses into [`ListRow`]s, skipping any that are not a
/// `LIST` (the transport may collect interleaved untagged data).
pub(crate) fn parse_list(lines: &[Vec<u8>]) -> ImapResult<Vec<ListRow>> {
    let mut rows = Vec::new();
    for line in lines {
        let items = items_of(line)?;
        // `LIST (attrs) delim name`
        let [keyword, attrs, delim, name, ..] = items.as_slice() else {
            continue;
        };
        if !keyword
            .as_atom()
            .is_some_and(|a| a.eq_ignore_ascii_case("LIST"))
        {
            continue;
        }
        let attributes = attrs
            .as_list()
            .unwrap_or(&[])
            .iter()
            .filter_map(|i| i.as_atom().map(str::to_owned))
            .collect();
        let delimiter = delim.as_nstring();
        let Some(name) = name.as_nstring() else {
            continue;
        };
        rows.push(ListRow {
            attributes,
            delimiter,
            name,
        });
    }
    Ok(rows)
}

/// Reads `UID FETCH` untagged responses into [`FetchRow`]s. Rows without a `UID`
/// (e.g. an unsolicited flag-only `FETCH`) are skipped, never errored.
pub(crate) fn parse_fetch(lines: &[Vec<u8>]) -> ImapResult<Vec<FetchRow>> {
    let mut rows = Vec::new();
    for line in lines {
        let items = items_of(line)?;
        // `<seq> FETCH (k v k v ...)`
        let [_seq, keyword, body, ..] = items.as_slice() else {
            continue;
        };
        if !keyword
            .as_atom()
            .is_some_and(|a| a.eq_ignore_ascii_case("FETCH"))
        {
            continue;
        }
        let Some(pairs) = body.as_list() else {
            continue;
        };
        if let Some(row) = fetch_row(pairs) {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Interprets a `FETCH` body's `key value` pairs into a [`FetchRow`]; `None` if no
/// `UID` is present.
fn fetch_row(pairs: &[Item]) -> Option<FetchRow> {
    let mut uid = None;
    let mut flags = Vec::new();
    let mut internal_date = None;
    let mut size = None;
    let mut envelope = None;
    let mut iter = pairs.iter();
    while let Some(key) = iter.next() {
        let Some(key) = key.as_atom() else { continue };
        let Some(value) = iter.next() else { break };
        match key.to_ascii_uppercase().as_str() {
            "UID" => uid = value.as_atom().and_then(|a| a.parse().ok()),
            "FLAGS" => {
                flags = value
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|i| i.as_atom().map(str::to_owned))
                    .collect();
            }
            "INTERNALDATE" => internal_date = value.as_nstring(),
            "RFC822.SIZE" => size = value.as_atom().and_then(|a| a.parse().ok()),
            "ENVELOPE" => envelope = value.as_list().map(envelope_of),
            _ => {}
        }
    }
    uid.map(|uid| FetchRow {
        uid,
        flags,
        internal_date,
        size,
        envelope,
    })
}

/// Interprets an `ENVELOPE` list's ten positional fields (RFC 9051 §7.5.2).
fn envelope_of(fields: &[Item]) -> Envelope {
    let at = |i: usize| fields.get(i);
    let addrs = |i: usize| at(i).map(addresses_of).unwrap_or_default();
    Envelope {
        subject: at(1).and_then(Item::as_nstring),
        from: addrs(2),
        sender: addrs(3),
        reply_to: addrs(4),
        to: addrs(5),
        cc: addrs(6),
        bcc: addrs(7),
        in_reply_to: at(8).and_then(Item::as_nstring),
        message_id: at(9).and_then(Item::as_nstring),
    }
}

/// Interprets an address-list item (`((name adl mailbox host) ...)` or `NIL`).
fn addresses_of(item: &Item) -> Vec<Address> {
    let Some(list) = item.as_list() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|addr| {
            let parts = addr.as_list()?;
            Some(Address {
                name: parts.first().and_then(Item::as_nstring),
                mailbox: parts.get(2).and_then(Item::as_nstring),
                host: parts.get(3).and_then(Item::as_nstring),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
