//! IMAP modified UTF-7 (RFC 3501 §5.1.3) ⇄ UTF-8: the mailbox-name encoding of the
//! **IMAP4rev1 wire**, and of nothing above it.
//!
//! A rev1 `LIST` reply carries mailbox names in a 7-bit encoding of its own: printable
//! ASCII stands for itself except `&`, which is written `&-`; everything else is the
//! modified BASE64 of the name's UTF-16BE code units between a `&` and a `-`, with `,` in
//! place of `/` and no `=` padding. Undecoded, a folder called `Travel & Expenses` reads as
//! `Travel &- Expenses`, and one named `日本語` as `&ZeVnLIqe-`. IMAP4rev2 dispenses with it
//! entirely: names are UTF-8 (RFC 9051 §5.1, Appendix E item 16).
//!
//! **The encoding stops at the transport.** A [`Mailbox`](engine_core::mail::Mailbox)'s id
//! and name are both the decoded UTF-8 form on either dialect, so a message key built from
//! one is the same key whichever revision the session negotiated. [`crate::transport`]
//! encodes on the way out and [`crate::mail`] decodes on the way in, each only when the
//! session is rev1. Keeping the wire form as the identity instead — as this crate once did
//! — makes a folder's id, and every message key inside it, change the day its server starts
//! offering rev2.

/// Decodes an IMAP modified-UTF-7 mailbox name for display.
///
/// Lenient by construction: mail is hostile input and a mailbox list must never fail to
/// parse over one odd name, so anything malformed — an unterminated `&`, a BASE64 run that
/// is not valid base64, code units that are not valid UTF-16 — yields that run **verbatim**
/// rather than an error or a replacement character. A server that (wrongly, but commonly)
/// sends raw UTF-8 therefore passes through untouched, since UTF-8 names carry no `&`.
pub(crate) fn decode(name: &str) -> String {
    // The common case is a pure-ASCII name with no shift at all: borrow-free and allocation
    // free to check, and it skips the whole scan below.
    if !name.contains('&') {
        return name.to_owned();
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(shift) = rest.find('&') {
        out.push_str(&rest[..shift]);
        let after = &rest[shift + 1..];
        // `&-` is the escape for a literal ampersand.
        if let Some(tail) = after.strip_prefix('-') {
            out.push('&');
            rest = tail;
            continue;
        }
        let Some((encoded, tail)) = after.split_once('-') else {
            // An unterminated shift: the rest of the name is not a BASE64 run.
            out.push('&');
            out.push_str(after);
            return out;
        };
        if let Some(decoded) = decode_base64_run(encoded) {
            out.push_str(&decoded);
        } else {
            // Not decodable: keep the run exactly as the server sent it.
            out.push('&');
            out.push_str(encoded);
            out.push('-');
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Decodes one modified-BASE64 run (the bytes between `&` and `-`) into UTF-8, or `None`
/// if it is not a well-formed run: the alphabet swaps `,` for `/`, the octets are UTF-16BE
/// code units, and an odd octet count or an unpaired surrogate is malformed.
fn decode_base64_run(encoded: &str) -> Option<String> {
    if encoded.is_empty() {
        return None;
    }
    let standard: String = encoded
        .chars()
        .map(|c| if c == ',' { '/' } else { c })
        .collect();
    let octets = crate::base64::decode(&standard)?;
    if octets.len() % 2 != 0 {
        return None;
    }
    let units = octets
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// Encodes a UTF-8 mailbox name into IMAP modified UTF-7, for a rev1 wire.
///
/// The exact inverse of [`decode`] for every name [`decode`] can produce, which is the
/// property that lets a `Mailbox`'s decoded id address the mailbox it came from. Only two
/// things are special: `&` becomes `&-`, and any run of non-printable-ASCII characters
/// becomes one `&…-` BASE64 shift. Printable ASCII (`0x20`–`0x7e`) is left alone, so an
/// all-ASCII name allocates nothing beyond the copy.
pub(crate) fn encode(name: &str) -> String {
    if name.is_ascii() && !name.contains('&') {
        return name.to_owned();
    }
    let mut out = String::with_capacity(name.len());
    // The pending run of characters that need a shift, held as UTF-16 code units.
    let mut shift: Vec<u16> = Vec::new();
    for ch in name.chars() {
        if matches!(ch, '\u{20}'..='\u{7e}') {
            flush_shift(&mut shift, &mut out);
            out.push(ch);
            // `&` is the shift introducer, so a literal one is written as the empty shift.
            if ch == '&' {
                out.push('-');
            }
        } else {
            let mut buf = [0u16; 2];
            shift.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    flush_shift(&mut shift, &mut out);
    out
}

/// Writes any pending shift run as `&<modified-base64>-` and clears it.
fn flush_shift(shift: &mut Vec<u16>, out: &mut String) {
    if shift.is_empty() {
        return;
    }
    let octets: Vec<u8> = shift.drain(..).flat_map(u16::to_be_bytes).collect();
    out.push('&');
    // The modified alphabet swaps `,` for `/`, and drops `=` padding entirely.
    for byte in crate::base64::encode(&octets).bytes() {
        match byte {
            b'=' => {}
            b'/' => out.push(','),
            other => out.push(char::from(other)),
        }
    }
    out.push('-');
}

#[cfg(test)]
#[path = "utf7_tests.rs"]
mod tests;
