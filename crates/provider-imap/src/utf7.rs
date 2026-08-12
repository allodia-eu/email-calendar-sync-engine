//! IMAP modified UTF-7 (RFC 3501 §5.1.3) → UTF-8, for **display names only**.
//!
//! A `LIST` reply carries mailbox names in a 7-bit encoding of its own: printable ASCII
//! stands for itself except `&`, which is written `&-`; everything else is the modified
//! BASE64 of the name's UTF-16BE code units between a `&` and a `-`, with `,` in place of
//! `/` and no `=` padding. Undecoded, a folder called `Travel & Expenses` reads as
//! `Travel &- Expenses`, and one named `日本語` reads as `&ZeVnLIqe-`.
//!
//! **Decode only, and only into [`Mailbox::name`](engine_core::mail::Mailbox::name).** The
//! encoded form is the name the *protocol* uses: it is what `SELECT`, `APPEND` and `LIST`
//! take, and it is what a [`MailboxId`](engine_core::ids::MailboxId) and every message key
//! built from one embed. Decoding it there instead would change the identity of every
//! message in a non-ASCII folder and hand `SELECT` a name the server never advertised, so
//! the wire name stays the id and the decoded name is what a human reads. That split is
//! also why there is no encoder here: no name this crate sends originates from a decoded
//! one.

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

#[cfg(test)]
#[path = "utf7_tests.rs"]
mod tests;
