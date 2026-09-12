//! RFC 2047 "encoded-word" decoding for header text (subjects, display names).
//!
//! JMAP and Graph hand the engine header text already decoded, so their adapters never
//! call this. The adapters that read RFC 5322 headers themselves do: IMAP's `ENVELOPE`
//! carries the header text verbatim, and Gmail's `payload.headers` returns the raw value,
//! so a non-ASCII subject reaches them as `=?UTF-8?Q?Caf=C3=A9?=`.
//!
//! The `B` (base64) and `Q` (quoted-printable) encodings are decoded here; the charset is
//! read with `mail-parser`'s table, the same one body extraction uses, so a header and a
//! body cannot disagree about what `iso-2022-jp` means. Per RFC 2047 §6.2, linear
//! whitespace *between* two adjacent encoded-words is removed (a word may be split
//! mid-character). Malformed input is passed through verbatim — header text is hostile
//! input and must never panic (`north-star.md`).

use mail_parser::decoders::{base64::base64_decode, charsets::map::charset_decoder};

/// Decodes any RFC 2047 encoded-words in `input`, leaving ordinary text untouched.
#[must_use]
pub fn decode(input: &str) -> String {
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
        "B" => base64_decode(encoded.as_bytes())?,
        "Q" => q_decode(encoded),
        _ => return None,
    };
    let consumed = charset_end + 1 + encoding_end + 1 + text_end + 2;
    Some((decode_charset(charset, &bytes), consumed))
}

/// Interprets bytes per the (case-insensitive) charset; a `*language` suffix
/// (RFC 2231) is ignored, and a charset `mail-parser` does not know falls back to a
/// UTF-8-lossy read.
///
/// Delegating the table rather than keeping one here is what makes the legacy charsets
/// real mail still carries work at all: the stateful `ISO-2022-*` sets, `Shift_JIS`,
/// `EUC-JP`/`-KR`, `GB18030`, `Big5`, and the single-byte `ISO-8859-*` / `windows-125*` /
/// `KOI8-*`. A 7-bit set is the trap — every `ISO-2022-JP` byte is valid ASCII, so a
/// UTF-8 read of one yields no replacement character to notice, just the escape sequences
/// as text.
///
/// The one label not taken at face value is `ISO-8859-1`, which is asked for as its
/// `Windows-1252` superset. The two agree on `0xA0..=0xFF`, and the `0x80..=0x9F` range
/// true Latin-1 leaves as C1 controls almost always carries CP1252 punctuation (smart
/// quotes, en/em dashes, `€`) in real mail — the lenient mapping browsers use (WHATWG
/// Encoding: the `iso-8859-1` label *is* `windows-1252`). Without it an Outlook subject's
/// en-dash (`0x96`) decodes to an unrenderable `\u{96}`. Stated here rather than left to
/// the table, because the table has already changed its answer once: `mail-parser` reads
/// this label as true Latin-1 at the pinned 0.11.4 and as CP1252 by 0.11.9.
fn decode_charset(charset: &str, bytes: &[u8]) -> String {
    let name = charset.split('*').next().unwrap_or(charset);
    let lowercased = name.to_ascii_lowercase();
    let name = match lowercased.as_str() {
        "iso-8859-1" | "iso_8859-1" | "latin1" | "l1" => "windows-1252",
        _ => name,
    };
    match charset_decoder(name.as_bytes()) {
        Some(decode) => decode(bytes),
        None => String::from_utf8_lossy(bytes).into_owned(),
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
    fn windows_1252_smart_punctuation_decodes() {
        // The real-world regression: an Outlook-style subject whose en-dash is CP1252
        // 0x96 — a UTF-8-lossy read mangles it to the replacement character.
        assert_eq!(
            decode("=?Windows-1252?Q?Welcome_to_TAC_Security_=96_Tier_2?="),
            "Welcome to TAC Security – Tier 2"
        );
        // Smart quotes (0x91/0x92), em-dash (0x97), and the euro sign (0x80) too.
        assert_eq!(decode("=?windows-1252?Q?=91hi=92_=97_=80?="), "‘hi’ — €");
        // The `iso-8859-1` label is treated as its CP1252 superset (browser behavior),
        // so a mislabeled 0x96 still decodes to an en-dash, while 0xA0..=0xFF are
        // unchanged from Latin-1.
        assert_eq!(decode("=?iso-8859-1?Q?a=96b=E9?="), "a–bé");
    }

    #[test]
    fn iso_2022_jp_decodes() {
        // Observed on real Japanese mail: a 7-bit stateful encoding, so every byte is
        // valid ASCII and a UTF-8 read produces no replacement character to notice —
        // just `$B...(B` where the text should be. Subject and display name from one
        // message, `B`-encoded as such mail always is.
        assert_eq!(
            decode("=?iso-2022-jp?b?GyRCPzckNyQkPnBKcyRyJCpDTiRpJDskNyReJDkbKEI=?="),
            "新しい情報をお知らせします"
        );
        assert_eq!(
            decode("=?iso-2022-jp?b?GyRCJSslOSU/JV4hPCU1JV0hPCVIGyhC?="),
            "カスタマーサポート"
        );
    }

    #[test]
    fn other_legacy_charsets_decode() {
        // One per family, so a table swap that drops a family fails here.
        assert_eq!(decode("=?shift_jis?B?g1SDfIFbg2c=?="), "サポート");
        assert_eq!(decode("=?euc-jp?B?xvzL3A==?="), "日本");
        assert_eq!(decode("=?koi8-r?B?8NLJ18XU?="), "Привет");
        assert_eq!(decode("=?gb2312?B?xOO6ww==?="), "你好");
        assert_eq!(decode("=?big5?B?p0Gmbg==?="), "你好");
    }

    #[test]
    fn an_unknown_charset_falls_back_to_utf8() {
        assert_eq!(decode("=?x-made-up?Q?Caf=C3=A9?="), "Café");
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
