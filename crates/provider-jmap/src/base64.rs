//! Standard base64 decoding (RFC 4648 §4), for the two places JMAP hands bytes over as text:
//! a `data:` URI in a contact's media, and `Blob/get`'s `data:asBase64` (RFC 9404).
//!
//! A body warm decodes every message it batches through here, so this is written to be cheap
//! per byte: a `match` rather than a search of the alphabet, and one allocation sized up front.

/// Decodes standard base64, skipping ASCII whitespace and stopping at the first `=` padding;
/// `None` on any byte outside the alphabet, since this is server-supplied input.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in text.as_bytes() {
        if byte == b'=' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        buffer = (buffer << 6) | u32::from(value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).expect("masked to a byte"));
        }
    }
    Some(out)
}

fn value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn decodes_the_rfc_4648_vectors() {
        for (encoded, decoded) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(decode(encoded).unwrap(), decoded.as_bytes(), "{encoded}");
        }
    }

    #[test]
    fn whitespace_is_skipped_and_a_foreign_byte_refused() {
        assert_eq!(decode("Zm9v\r\nYmFy").unwrap(), b"foobar");
        assert_eq!(decode("Zm9v!"), None);
    }
}
