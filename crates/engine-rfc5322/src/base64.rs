//! RFC 4648 base64 encoding for MIME bodies and RFC 2047 `B` encoded-words.
//!
//! Encode only: this crate assembles outbound messages, so it never decodes. The
//! matching decoder lives with each parser (`engine-mime`, `provider-imap`'s inbound
//! header decoder).

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
    fn encodes_arbitrary_bytes_in_76_char_safe_chunks() {
        // A binary blob (the attachment path) encodes to the exact RFC 4648 output.
        assert_eq!(encode(&[0, 1, 2, 3, 4, 5]), "AAECAwQF");
    }
}
