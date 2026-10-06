//! Pulling the raw `BODY[]` payload out of a `FETCH` response.
//!
//! Split from [`crate::parse`] because it works differently: the payload is arbitrary
//! RFC 5322 bytes rather than IMAP grammar, so this scans the `{n}` framing around it
//! instead of tokenizing. What the framing must prove is that the bytes belong to the
//! UID that was asked for.

/// Extracts the raw `BODY[]` literal bytes for `expected_uid` from a
/// `UID FETCH <uid> (BODY.PEEK[])` response (RFC 9051 §7.5.2), or `None` if no line
/// carries a `BODY[]` literal **for that UID**.
///
/// The server echoes the section as `BODY[] {n}` with the `n` raw bytes inlined by
/// the transport, so we scan for that framing rather than tokenizing (the payload is
/// arbitrary RFC 5322 bytes, not IMAP grammar). Each candidate line is required to
/// carry `UID <expected_uid>` in the framing on **either** side of its `BODY[]` payload,
/// so an unsolicited `FETCH` for a different UID (a concurrent flag update the server
/// piggybacks) can never supply the wrong message's bytes. `None` means the UID returned no body —
/// the caller treats that as an expunge (re-sync), not a parse error.
pub(crate) fn parse_fetch_body(untagged: &[Vec<u8>], expected_uid: u32) -> Option<Vec<u8>> {
    untagged
        .iter()
        .find_map(|line| extract_body_literal(line, expected_uid))
}

/// The UID and `BODY[]` bytes one `FETCH` line carries, for a fetch over a UID set whose
/// rows arrive in whatever order the server sends them. `None` when the line has no `BODY[]`
/// literal (a flag update the server piggybacked) or its framing names no UID.
pub(crate) fn parse_any_fetch_body(line: &[u8]) -> Option<(u32, Vec<u8>)> {
    let (before, payload, tail) = split_body_literal(line)?;
    let uid = uidfetch_number(before)
        .or_else(|| first_uid(before))
        .or_else(|| first_uid(tail))?;
    Some((uid, payload.to_vec()))
}

/// Pulls the `{n}`-framed bytes that follow the first `BODY[]` marker in `line`,
/// provided the framing before that marker names `expected_uid`. `None` if the line
/// is for another UID, or the framing is absent or truncated.
fn extract_body_literal(line: &[u8], expected_uid: u32) -> Option<Vec<u8>> {
    let (before, payload, tail) = split_body_literal(line)?;
    if uidfetch_number(before) != Some(expected_uid)
        && !names_uid(before, expected_uid)
        && !names_uid(tail, expected_uid)
    {
        return None;
    }
    Some(payload.to_vec())
}

/// Splits a `FETCH` line around its `BODY[]` literal: the framing before the marker, the
/// payload, and the framing after it.
///
/// The `UID <n>` pair is framing, and framing sits on **both** sides of the payload: RFC 9051
/// orders no item of a `FETCH` response, and a `UID FETCH` obliges the server to return a UID
/// whether or not it was asked for, which a server that appends rather than prepends puts
/// after the literal. Proton Mail Bridge answers `* 1449 FETCH (BODY[] {65888}\r\n<payload>
/// UID 1449)`, so reading only the prefix finds no UID on any message and reports every body
/// as gone. The payload between them is never searched, so a message whose own bytes carry
/// "UID 7" cannot pass itself off as the framing that names it.
fn split_body_literal(line: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    const MARKER: &[u8] = b"BODY[]";
    let marker_at = find_subsequence(line, MARKER)?;
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
    if body.len() < len {
        return None;
    }
    let (payload, tail) = body.split_at(len);
    Some((&line[..marker_at], payload, tail))
}

/// The UID that numbers a `<uid> UIDFETCH (…)` line (RFC 9586), which need not repeat it as
/// a `UID` item.
fn uidfetch_number(framing: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(framing).ok()?;
    let mut words = text.split_ascii_whitespace();
    let number = words.next()?.parse().ok()?;
    let keyword = words.next()?;
    let keyword = keyword.strip_suffix('(').unwrap_or(keyword);
    keyword.eq_ignore_ascii_case("UIDFETCH").then_some(number)
}

/// The first `UID <n>` a stretch of framing names.
fn first_uid(framing: &[u8]) -> Option<u32> {
    const UID: &[u8] = b"UID ";
    let upper = framing.to_ascii_uppercase();
    let start = find_subsequence(&upper, UID)? + UID.len();
    let digits: Vec<u8> = upper[start..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// `true` if `framing` contains a `UID <expected>` token (case-insensitive `UID`,
/// the decimal matched as a whole number so `UID 70` does not satisfy `7`).
///
/// Only ever given framing, never a message payload: see [`extract_body_literal`].
fn names_uid(framing: &[u8], expected: u32) -> bool {
    const UID: &[u8] = b"UID ";
    let upper = framing.to_ascii_uppercase();
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
#[path = "parse_body_tests.rs"]
mod tests;
