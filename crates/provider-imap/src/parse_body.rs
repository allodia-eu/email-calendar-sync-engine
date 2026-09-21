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

/// Pulls the `{n}`-framed bytes that follow the first `BODY[]` marker in `line`,
/// provided the framing before that marker names `expected_uid`. `None` if the line
/// is for another UID, or the framing is absent or truncated.
fn extract_body_literal(line: &[u8], expected_uid: u32) -> Option<Vec<u8>> {
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
    // The `UID <n>` pair is FETCH framing, and framing sits on **both** sides of the
    // payload: RFC 9051 orders no item of a `FETCH` response, and a `UID FETCH` obliges the
    // server to return a UID whether or not it was asked for, which a server that appends
    // rather than prepends puts after the literal. Proton Mail Bridge answers
    // `* 1449 FETCH (BODY[] {65888}\r\n<payload> UID 1449)`, so reading only the prefix finds
    // no UID on any message and reports every body as gone.
    //
    // The payload between them is never searched, so a message whose own bytes carry
    // "UID 7" cannot pass itself off as the framing that names it.
    let (payload, tail) = body.split_at(len);
    if !names_uid(&line[..marker_at], expected_uid) && !names_uid(tail, expected_uid) {
        return None;
    }
    Some(payload.to_vec())
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
