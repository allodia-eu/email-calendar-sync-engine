//! RFC 2047 "encoded-word" decoding for header text (subjects, display names).
//!
//! IMAP `ENVELOPE` carries header text verbatim, so a non-ASCII subject arrives as
//! `=?UTF-8?Q?Caf=C3=A9?=`. This decodes those words to text. It handles the `B`
//! (base64) and `Q` (quoted-printable) encodings and the UTF-8 / ISO-8859-1
//! charsets (others fall back to a UTF-8-lossy read); per RFC 2047 §6.2, linear
//! whitespace *between* two adjacent encoded-words is removed (a word may be split
//! mid-character). Malformed input is passed through verbatim — header text is
//! hostile input and must never panic (`north-star.md`).

/// Decodes any RFC 2047 encoded-words in `input`, leaving ordinary text untouched.
pub(crate) fn decode(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    let mut prev_was_encoded = false;
    loop {
        let Some(idx) = rest.find("=?") else {
            out.push_str(rest);
            return out;
        };
        let before = &rest[..idx];
        let parsed = parse_encoded_word(&rest[idx + 2..]);
        // Whitespace between two adjacent encoded-words is dropped (RFC 2047 §6.2).
        let drop_ws = prev_was_encoded
            && parsed.is_some()
            && !before.is_empty()
            && before.chars().all(|c| c == ' ' || c == '\t');
        if !drop_ws {
            out.push_str(before);
        }
        if let Some((decoded, consumed)) = parsed {
            out.push_str(&decoded);
            rest = &rest[idx + 2 + consumed..];
            prev_was_encoded = true;
        } else {
            out.push_str("=?");
            rest = &rest[idx + 2..];
            prev_was_encoded = false;
        }
    }
}

/// Parses one encoded-word body (the text *after* the leading `=?`): returns the
/// decoded text and how many bytes it consumed (through the closing `?=`), or
/// `None` if it is not a well-formed encoded-word.
fn parse_encoded_word(body: &str) -> Option<(String, usize)> {
    let charset_end = body.find('?')?;
    let charset = &body[..charset_end];
    let after_charset = &body[charset_end + 1..];
    let encoding_end = after_charset.find('?')?;
    let encoding = &after_charset[..encoding_end];
    let text = &after_charset[encoding_end + 1..];
    let text_end = text.find("?=")?;
    let encoded = &text[..text_end];

    let bytes = match encoding.to_ascii_uppercase().as_str() {
        "B" => crate::base64::decode(encoded)?,
        "Q" => q_decode(encoded),
        _ => return None,
    };
    let consumed = charset_end + 1 + encoding_end + 1 + text_end + 2;
    Some((decode_charset(charset, &bytes), consumed))
}

/// Interprets bytes per the (case-insensitive) charset; a `*language` suffix
/// (RFC 2231) is ignored, and unknown charsets fall back to a UTF-8-lossy read.
fn decode_charset(charset: &str, bytes: &[u8]) -> String {
    let name = charset
        .split('*')
        .next()
        .unwrap_or(charset)
        .to_ascii_uppercase();
    match name.as_str() {
        "ISO-8859-1" | "LATIN1" => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Quoted-printable decoding for the `Q` encoding: `_` is a space, `=XX` is a hex
/// byte; a malformed `=` is kept literally.
fn q_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => out.push(b' '),
            b'=' => {
                if let (Some(hi), Some(lo)) = (
                    bytes.get(i + 1).copied().and_then(hex_value),
                    bytes.get(i + 2).copied().and_then(hex_value),
                ) {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
                out.push(b'=');
            }
            other => out.push(other),
        }
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(decode("Just a normal subject"), "Just a normal subject");
        assert_eq!(decode(""), "");
    }

    #[test]
    fn q_encoded_utf8_decodes() {
        // "Café" with a quoted-printable UTF-8 é.
        assert_eq!(decode("=?UTF-8?Q?Caf=C3=A9?="), "Café");
        // `_` is a space.
        assert_eq!(decode("=?UTF-8?Q?a_b?="), "a b");
    }

    #[test]
    fn b_encoded_utf8_decodes() {
        // base64("Café") = "Q2Fmw6k=".
        assert_eq!(decode("=?UTF-8?B?Q2Fmw6k=?="), "Café");
    }

    #[test]
    fn whitespace_between_adjacent_words_is_dropped() {
        // A word ("good") is split across two encoded-words, so the whitespace
        // between them must be removed; the em-dash exercises a multi-byte char.
        let input = "=?UTF-8?Q?Status_=E2=80=94_all_go?= =?UTF-8?Q?od?=";
        assert_eq!(decode(input), "Status — all good");
    }

    #[test]
    fn text_around_an_encoded_word_is_preserved() {
        assert_eq!(decode("Re: =?UTF-8?Q?Caf=C3=A9?= today"), "Re: Café today");
    }

    #[test]
    fn iso_8859_1_maps_bytes_to_latin1() {
        // 0xE9 is é in Latin-1.
        assert_eq!(decode("=?ISO-8859-1?Q?Caf=E9?="), "Café");
    }

    #[test]
    fn malformed_words_pass_through_without_panicking() {
        for bad in [
            "=?",
            "=?UTF-8?",
            "=?UTF-8?Q?unterminated",
            "=?UTF-8?Z?bad-encoding?=",
            "=?UTF-8?B?not valid base64!?=",
            "=?UTF-8?Q?=?=",
            "a =? b ?= c",
        ] {
            // Must return *something* and never panic; exact output is unspecified.
            let _ = decode(bad);
        }
        // A bad encoding letter leaves the word literal.
        assert_eq!(decode("=?UTF-8?Z?x?="), "=?UTF-8?Z?x?=");
    }
}
