//! RFC 2047 "encoded-word" decoding for header text (subjects, display names).
//!
//! JMAP and Graph hand the engine header text already decoded, so their adapters never
//! call this. The adapters that read RFC 5322 headers themselves do: IMAP's `ENVELOPE`
//! carries the header text verbatim, and Gmail's `payload.headers` returns the raw value,
//! so a non-ASCII subject reaches them as `=?UTF-8?Q?Caf=C3=A9?=`.
//!
//! The scanning and the `B` (base64) and `Q` (quoted-printable) payloads are `encodify`'s
//! RFC 2047 decoder, the one mail-parser runs on the headers of a parsed message; the
//! charset is read with `mail-parser`'s table, the same one body extraction uses, so a
//! header and a body cannot disagree about what `iso-2022-jp` means. Per RFC 2047 §6.2,
//! linear whitespace *between* two adjacent encoded-words is removed, and adjacent words in
//! one charset are joined before conversion, so a character split across two words decodes.
//! Folding whitespace becomes one space and the ends are trimmed. Malformed words are passed
//! through verbatim — header text is hostile input and must never panic (`north-star.md`).

use mail_parser::decoders::charsets;

/// Decodes any RFC 2047 encoded-words in `input`, leaving ordinary text untouched.
#[must_use]
pub fn decode(input: &str) -> String {
    encodify::rfc2047::decode_text(input.as_bytes(), decode_charset).into_owned()
}

/// Appends `bytes`, read in `charset`, to `out`; a charset `mail-parser` does not know falls
/// back to a UTF-8-lossy read. `encodify` hands over the label without its RFC 2231
/// `*language` suffix.
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
/// the table, because the table has already changed its answer once: `mail-parser` read
/// this label as true Latin-1 at 0.11.4 and reads it as CP1252 from 0.11.5 on.
fn decode_charset(charset: &[u8], bytes: &[u8], out: &mut String) {
    let charset: &[u8] = if [&b"iso-8859-1"[..], b"iso_8859-1", b"latin1", b"l1"]
        .iter()
        .any(|label| charset.eq_ignore_ascii_case(label))
    {
        b"windows-1252"
    } else {
        charset
    };
    charsets::decode_append(charset, bytes, out);
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
        assert_eq!(decode("=?UTF-8?Q?unterminated"), "=?UTF-8?Q?unterminated");
    }

    #[test]
    fn a_character_split_across_two_words_decodes() {
        // RFC 2047 §5 forbids the split, and senders make it anyway: the em-dash's three
        // bytes span two words. Decoding each word on its own leaves two U+FFFD.
        assert_eq!(decode("=?UTF-8?Q?=E2=80?= =?UTF-8?Q?=94?="), "—");
        assert_eq!(decode("=?UTF-8?B?4oA=?= =?UTF-8?B?lA==?="), "—");
    }
}
