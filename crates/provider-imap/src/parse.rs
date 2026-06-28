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
//! (`NIL` / atom / quoted-string / `{n}` literal / parenthesized list), defined in
//! [`crate::tokenize`]; each response is then read off the resulting [`Item`] tree.

use crate::error::{ImapError, ImapResult};
use crate::tokenize::{Item, items_of};

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
    /// `HIGHESTMODSEQ` — the mailbox's current mod-sequence (RFC 7162 §3.1.2.1),
    /// present only when the mailbox is opened with CONDSTORE/QRESYNC enabled. It is
    /// the baseline a subsequent QRESYNC delta carries forward in its cursor.
    pub highest_modseq: Option<u64>,
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

/// One row of a `UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE
/// BODY.PEEK[HEADER.FIELDS (REFERENCES)])`.
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
    /// The raw `References` header line from `BODY[HEADER.FIELDS (REFERENCES)]`
    /// (e.g. `"References: <a@x> <b@y>\r\n\r\n"`), if requested and present.
    /// `None` (or empty) when the message carries no `References`.
    pub references: Option<String>,
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
/// `[UIDVALIDITY 12345]` or `[HIGHESTMODSEQ 42]` (RFC 9051 §7.1, RFC 7162), parsed
/// into the requested integer width (`u32` for UID-space codes, `u64` for a MODSEQ).
fn response_code<T: std::str::FromStr>(line: &[u8], code: &str) -> Option<T> {
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
    let mut highest_modseq = None;
    for line in lines {
        if let Some(v) = response_code(line, "UIDVALIDITY") {
            uid_validity = Some(v);
        }
        if let Some(v) = response_code(line, "UIDNEXT") {
            uid_next = Some(v);
        }
        if let Some(m) = response_code(line, "HIGHESTMODSEQ") {
            highest_modseq = Some(m);
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
        highest_modseq,
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

/// Reads a `UID SEARCH` result into its matched UIDs. Handles both the classic
/// untagged `SEARCH <n> <n> …` response (RFC 9051 §7.3.4) and the extended
/// `ESEARCH … ALL <sequence-set>` form a server may return instead: in both, the
/// numeric tokens are collected (an `ESEARCH` range like `1:3` contributes its
/// endpoints, which is enough for the lowest/highest UID the caller needs). Lines
/// that are not a search result are skipped; an absent or zero-match result yields an
/// empty vec.
pub(crate) fn parse_search(lines: &[Vec<u8>]) -> Vec<u32> {
    for line in lines {
        let text = String::from_utf8_lossy(line);
        let mut tokens = text.split_whitespace();
        let Some(head) = tokens.next() else { continue };
        if head.eq_ignore_ascii_case("SEARCH") {
            // `SEARCH 3 7 9` — every remaining token is a matched UID.
            return tokens.filter_map(|token| token.parse().ok()).collect();
        }
        if head.eq_ignore_ascii_case("ESEARCH") {
            // `ESEARCH (TAG "a3") UID ALL 1:3,7` — the set follows the `ALL` label.
            return esearch_all_uids(tokens);
        }
    }
    Vec::new()
}

/// Collects the UID numbers from an `ESEARCH` response's `ALL <sequence-set>`: the
/// tokens after `ALL`, split on the set's `,` and range `:` separators. Range
/// endpoints (not the expanded interior) are returned — enough for the lowest UID the
/// window floor needs, and a close-enough count for the progress denominator.
fn esearch_all_uids<'a>(mut tokens: impl Iterator<Item = &'a str>) -> Vec<u32> {
    let found_all = tokens
        .by_ref()
        .any(|token| token.eq_ignore_ascii_case("ALL"));
    if !found_all {
        return Vec::new();
    }
    let Some(set) = tokens.next() else {
        return Vec::new();
    };
    set.split(',')
        .flat_map(|range| range.split(':'))
        .filter_map(|number| number.parse().ok())
        .collect()
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
    let mut references = None;
    let mut iter = pairs.iter();
    while let Some(key) = iter.next() {
        let Some(key) = key.as_atom() else { continue };
        // A body-section item (`BODY[HEADER.FIELDS (REFERENCES)] <value>`) does not
        // tokenize as a single atom key: the section spec's brackets, list, and
        // spaces split it into `BODY[HEADER.FIELDS` + `(REFERENCES)` + `]` before the
        // value. Recognize it structurally by the `BODY[` prefix, drain the rest of
        // the section spec up to its closing `]` atom, then read the value.
        if key.to_ascii_uppercase().starts_with("BODY[") {
            let value = drain_body_section(key, &mut iter);
            references = value.and_then(Item::as_nstring);
            continue;
        }
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
        references,
    })
}

/// Consumes the remainder of a `BODY[...]` section spec and returns the body value
/// that follows it. The `key` atom is the leading `BODY[...` fragment; if it does
/// not already contain the closing `]` (the common `BODY[HEADER.FIELDS (REFERENCES)]`
/// case, split into `BODY[HEADER.FIELDS` + `(REFERENCES)` + `]`), `iter` is advanced
/// over the spec items up to and including the `]` atom. The next item is the value.
fn drain_body_section<'a>(key: &str, iter: &mut std::slice::Iter<'a, Item>) -> Option<&'a Item> {
    if !key.contains(']') {
        // Drain spec items until the atom that ends with `]`.
        for item in iter.by_ref() {
            if item.as_atom().is_some_and(|a| a.ends_with(']')) {
                break;
            }
        }
    }
    iter.next()
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

/// Extracts the raw `BODY[]` literal bytes for `expected_uid` from a
/// `UID FETCH <uid> (BODY.PEEK[])` response (RFC 9051 §7.5.2), or `None` if no line
/// carries a `BODY[]` literal **for that UID**.
///
/// The server echoes the section as `BODY[] {n}` with the `n` raw bytes inlined by
/// the transport, so we scan for that framing rather than tokenizing (the payload is
/// arbitrary RFC 5322 bytes, not IMAP grammar). Each candidate line is required to
/// carry `UID <expected_uid>` before its `BODY[]` marker, so an unsolicited `FETCH`
/// for a different UID (a concurrent flag update the server piggybacks) can never
/// supply the wrong message's bytes. `None` means the UID returned no body — the
/// caller treats that as an expunge (re-sync), not a parse error.
pub(crate) fn parse_fetch_body(untagged: &[Vec<u8>], expected_uid: u32) -> Option<Vec<u8>> {
    untagged
        .iter()
        .find_map(|line| extract_body_literal(line, expected_uid))
}

/// Pulls the `{n}`-framed bytes that follow the first `BODY[]` marker in `line`,
/// provided the framing before that marker names `expected_uid`. `None` if the line
/// is for another UID, or the framing is absent or truncated.
fn extract_body_literal(line: &[u8], expected_uid: u32) -> Option<Vec<u8>> {
    const MARKER: &[u8] = b"BODY[]";
    let marker_at = find_subsequence(line, MARKER)?;
    // The `UID <n>` pair is part of the FETCH framing, which precedes the body
    // literal; restrict the UID check to that prefix so the payload bytes (which may
    // themselves contain "UID 7") cannot spoof it.
    if !prefix_names_uid(&line[..marker_at], expected_uid) {
        return None;
    }
    let after_marker = &line[marker_at + MARKER.len()..];
    // `BODY[] {n}\r\n<n bytes>`: skip the separating space, read the `{n}` length,
    // then take exactly the n bytes after the CRLF.
    let after_brace = after_marker
        .strip_prefix(b" ")
        .unwrap_or(after_marker)
        .strip_prefix(b"{")?;
    let close = after_brace.iter().position(|&b| b == b'}')?;
    let len: usize = std::str::from_utf8(&after_brace[..close])
        .ok()?
        .parse()
        .ok()?;
    let body = after_brace[close + 1..]
        .strip_prefix(b"\r\n")
        .or_else(|| after_brace[close + 1..].strip_prefix(b"\n"))?;
    (body.len() >= len).then(|| body[..len].to_vec())
}

/// `true` if `prefix` contains a `UID <expected>` token (case-insensitive `UID`,
/// the decimal matched as a whole number so `UID 70` does not satisfy `7`).
fn prefix_names_uid(prefix: &[u8], expected: u32) -> bool {
    const UID: &[u8] = b"UID ";
    let upper = prefix.to_ascii_uppercase();
    let mut from = 0;
    while let Some(rel) = find_subsequence(&upper[from..], UID) {
        let start = from + rel + UID.len();
        let digits: Vec<u8> = upper[start..]
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if std::str::from_utf8(&digits)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            == Some(expected)
        {
            return true;
        }
        from = start;
    }
    false
}

/// The first index at which `needle` occurs in `haystack`, if any.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
