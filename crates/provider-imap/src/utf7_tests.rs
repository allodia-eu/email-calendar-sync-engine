//! Modified UTF-7 decoding, including the two shapes that made a real folder list unreadable:
//! an escaped ampersand and a BASE64 run.

use super::decode;

#[test]
fn a_plain_ascii_name_is_unchanged() {
    assert_eq!(decode("INBOX"), "INBOX");
    assert_eq!(decode("Archive/2026"), "Archive/2026");
    assert_eq!(decode(""), "");
}

/// The reported symptom: a folder named with an ampersand rendered as `&-` in every client
/// surface, because nothing ever decoded the escape.
#[test]
fn an_escaped_ampersand_decodes_to_one_ampersand() {
    assert_eq!(decode("Belmar &- KBL"), "Belmar & KBL");
    assert_eq!(decode("&-"), "&");
    assert_eq!(decode("&-&-"), "&&");
}

/// RFC 3501 §5.1.3's own example, so the alphabet swap (`,` for `/`) and the UTF-16BE
/// interpretation are pinned to the spec rather than to our reading of it.
#[test]
fn the_rfc_example_decodes() {
    assert_eq!(
        decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
        "~peter/mail/台北/日本語"
    );
}

#[test]
fn a_run_outside_the_basic_plane_decodes_from_its_surrogate_pair() {
    // U+1F4E7 (📧) is two UTF-16 code units; a decoder that ignored surrogate pairing
    // would produce two replacement characters here.
    assert_eq!(decode("&2D3c5w-"), "📧");
}

#[test]
fn shifts_compose_with_surrounding_text() {
    assert_eq!(decode("Mail/&ZeVnLIqe-/Sent"), "Mail/日本語/Sent");
    assert_eq!(decode("&ZeVnLIqe-&-&U,BTFw-"), "日本語&台北");
}

/// A server that sends raw UTF-8 rather than modified UTF-7 (common, and not what the
/// grammar says) must pass through untouched — its bytes carry no `&` to misread.
#[test]
fn a_raw_utf8_name_passes_through() {
    assert_eq!(decode("Bücher"), "Bücher");
    assert_eq!(decode("日本語"), "日本語");
}

/// Every malformed shape yields the run verbatim. A mailbox list must survive one odd name:
/// failing, or substituting replacement characters, would lose folders the user has.
#[test]
fn malformed_runs_are_returned_verbatim() {
    // Unterminated shift.
    assert_eq!(decode("&"), "&");
    assert_eq!(decode("Sent &ZeVnLIqe"), "Sent &ZeVnLIqe");
    // Not base64 at all.
    assert_eq!(decode("&!!!-"), "&!!!-");
    assert_eq!(decode("&&-"), "&&-");
    // Valid base64, but an odd number of octets cannot be UTF-16BE code units.
    assert_eq!(decode("&AAAAA-"), "&AAAAA-");
    // A lone high surrogate is not decodable UTF-16.
    assert_eq!(decode("&2D0-"), "&2D0-");
}
