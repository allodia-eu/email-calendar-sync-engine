//! URL-safe base64 (RFC 4648 §5) for Gmail's `raw` message field.
//!
//! Gmail transports whole RFC 5322 messages as **base64url** JSON strings, both
//! ways: `users.messages.send` takes `{ "raw": "<base64url>" }`, and
//! `users.messages.get?format=raw` returns the source the same way. So — unlike the
//! other adapters, which either assemble (encode-only, `engine-rfc5322`) or fetch raw
//! bytes (Graph's `$value`) — this adapter needs a base64url codec on **both** sides,
//! and it lives here with its parser (the engine's convention: the decoder lives with
//! the thing that reads the bytes).
//!
//! [`encode`] emits the URL-safe alphabet with `=` padding (Gmail accepts it);
//! [`decode`] tolerates either alphabet, missing padding, and embedded whitespace, so
//! a value copied from a captured fixture round-trips regardless of how it was wrapped.

/// The URL-safe base64 alphabet (RFC 4648 §5): `+`/`/` replaced by `-`/`_`.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes `input` as URL-safe base64 with `=` padding.
pub(crate) fn encode(input: &[u8]) -> String {
    let symbol = |bits: u8| char::from(ALPHABET[usize::from(bits)]);
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(symbol(b0 >> 2));
        out.push(symbol(((b0 & 0x03) << 4) | (b1 >> 4)));
        out.push(if chunk.len() > 1 {
            symbol(((b1 & 0x0f) << 2) | (b2 >> 6))
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            symbol(b2 & 0x3f)
        } else {
            '='
        });
    }
    out
}

/// Decodes URL-safe (or standard) base64, ignoring padding and whitespace.
///
/// Returns `None` if a non-alphabet, non-padding, non-whitespace byte appears — a
/// malformed `raw` payload the caller surfaces as a protocol error rather than
/// silently truncating.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    // Accept both alphabets, so a fixture pasted in either form round-trips.
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in text.as_bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let v = value(byte)?;
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).expect("masked to a byte"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_rfc_4648_vectors_url_safe() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn url_safe_alphabet_uses_dash_and_underscore() {
        // Bytes that map to index 62/63 must render as `-`/`_`, never `+`/`/`.
        let encoded = encode(&[0xFB, 0xFF]); // 62 then 63 in the first two symbols
        assert!(encoded.starts_with("-_"), "{encoded}");
        assert!(!encoded.contains('+') && !encoded.contains('/'));
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        let blob: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode(&blob)).unwrap(), blob);
    }

    #[test]
    fn decodes_either_alphabet_and_tolerates_whitespace_and_missing_padding() {
        // Standard alphabet, no padding, with a newline (as a wrapped fixture might be).
        assert_eq!(decode("Zm9v\nYmFy").unwrap(), b"foobar");
        // URL-safe with padding stripped.
        assert_eq!(decode("Zm8").unwrap(), b"fo");
    }

    #[test]
    fn rejects_a_non_alphabet_byte() {
        assert!(decode("Zm9v*bad").is_none());
    }
}
