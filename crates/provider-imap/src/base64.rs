//! Shared RFC 4648 base64 codec.
//!
//! One alphabet, encode and decode together, so the two directions cannot drift.
//! Used by the SMTP `AUTH PLAIN` SASL token ([`crate::smtp`]) and RFC 2047
//! `B`-encoded header words ([`crate::encoded_word`]); these previously each
//! hand-rolled their own half.

/// The standard base64 alphabet (RFC 4648 §4).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `input` as standard base64 with `=` padding.
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

/// Decodes standard base64, stopping at the first `=` padding; `None` on any
/// non-alphabet byte (mail is hostile input — never panic).
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in text.as_bytes() {
        if byte == b'=' {
            break;
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

fn value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_rfc_4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decodes_the_rfc_4648_vectors_and_round_trips() {
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
        // Padding is tolerated and stops decoding.
        assert_eq!(decode("Zg==").unwrap(), b"f");
        for sample in [&b""[..], b"f", b"fo", b"foo", b"a \x00\xff blob"] {
            assert_eq!(
                decode(&encode(sample)).unwrap(),
                sample,
                "round-trip {sample:?}"
            );
        }
    }

    #[test]
    fn decode_rejects_a_non_alphabet_byte() {
        assert!(decode("not valid!").is_none());
    }
}
