//! `engine-mime` — MIME/RFC 5322 body extraction.
//!
//! One pure function, [`extract_body`], turns a message's raw RFC 5322 source
//! ([`RawMime`], the lossless Tier-3 blob the store caches on demand —
//! `north-star.md`) into a displayable [`MessageBody`]: the best `text/plain` and
//! `text/html` bodies, content-transfer- and charset-decoded to UTF-8.
//!
//! The decoding is delegated to [`mail_parser`] — a hardened, pure-Rust parser
//! (`Cargo.toml` records why we depend on it rather than hand-rolling MIME). Mail
//! is hostile input: malformed or truncated bytes yield an **empty** body, never a
//! panic (`north-star.md` security). The crate is I/O-free and async-free, so the
//! provider/store layers own *fetching* and *caching* the raw bytes and this layer
//! only *interprets* them.

use std::borrow::Cow;

use engine_core::mail::MessageBody;
use engine_core::raw::RawMime;
use mail_parser::{MessageParser, PartType};

/// Extracts the displayable [`MessageBody`] from a raw RFC 5322 message.
///
/// [`MessageBody::plain`] is the message's canonical text rendering — the
/// decoded `text/plain` body, or a text rendering of an HTML-only message — so a
/// plain-text reading view always has something to show. [`MessageBody::html`] is
/// captured only when the message carries a real `text/html` part (mail-parser maps
/// a text-only message's text part into its HTML body list too, so its presence
/// alone does not prove a real HTML part); it is **unsanitized** and a host must
/// sanitize before rendering.
///
/// A message that cannot be parsed, or carries no text part, yields
/// [`MessageBody::empty`].
#[must_use]
pub fn extract_body(raw: &RawMime) -> MessageBody {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return MessageBody::empty();
    };

    let plain = message.body_text(0).map(Cow::into_owned);
    // Take the decoded contents of the first body part that is *actually* a
    // `text/html` part, rather than `body_html(0)`. mail-parser lists a text-only
    // message's text part in its `html_body` index too (so `body_html` can
    // synthesize HTML from plain text), and for a `multipart/mixed` with a leading
    // text part before a `multipart/alternative`, `html_body[0]` is that leading
    // *text* part — so `body_html(0)` would return fabricated HTML and drop the real
    // one. Matching on `PartType::Html` captures genuine provider HTML only.
    let html = message.html_bodies().find_map(|part| match &part.body {
        PartType::Html(text) => Some(text.as_ref().to_owned()),
        _ => None,
    });

    MessageBody::new(plain, html)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: &[u8]) -> RawMime {
        RawMime::new(bytes)
    }

    #[test]
    fn plain_text_message() {
        let body = extract_body(&raw(
            b"From: a@b\r\nSubject: Hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello world\r\n",
        ));
        assert!(body.plain().unwrap().contains("Hello world"));
        assert_eq!(body.html(), None);
    }

    #[test]
    fn multipart_alternative_keeps_both() {
        let body = extract_body(&raw(
            b"Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\nplain part\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<p>html part</p>\r\n\
              --b--\r\n",
        ));
        assert!(body.plain().unwrap().contains("plain part"));
        assert!(body.html().unwrap().contains("html part"));
    }

    #[test]
    fn decodes_quoted_printable_utf8() {
        let body = extract_body(&raw(b"Content-Type: text/plain; charset=utf-8\r\n\
              Content-Transfer-Encoding: quoted-printable\r\n\r\nCaf=C3=A9 =3D test\r\n"));
        assert!(body.plain().unwrap().contains("Café = test"));
    }

    #[test]
    fn decodes_base64() {
        let body = extract_body(&raw(b"Content-Type: text/plain; charset=utf-8\r\n\
              Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8gQmFzZTY0\r\n"));
        assert!(body.plain().unwrap().contains("Hello Base64"));
    }

    #[test]
    fn decodes_legacy_charset() {
        // `=E9` is `é` in ISO-8859-1 (Latin-1), not UTF-8 — exercises the
        // `full_encoding` charset tables.
        let body = extract_body(&raw(b"Content-Type: text/plain; charset=iso-8859-1\r\n\
              Content-Transfer-Encoding: quoted-printable\r\n\r\nCaf=E9\r\n"));
        assert!(body.plain().unwrap().contains("Café"));
    }

    #[test]
    fn html_only_message_renders_text_and_keeps_html() {
        let body = extract_body(&raw(b"Content-Type: text/html; charset=utf-8\r\n\r\n\
              <html><body><b>Bold</b> text</body></html>\r\n"));
        // `body_text` falls back to a text rendering of the HTML.
        let plain = body.plain().unwrap();
        assert!(plain.contains("Bold") && plain.contains("text"), "{plain}");
        // The real HTML part is captured for the later sanitized-render slice.
        assert!(body.html().unwrap().contains("<b>Bold</b>"));
    }

    #[test]
    fn multipart_mixed_finds_text_past_attachment() {
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nthe body text\r\n\
              --m\r\nContent-Type: application/octet-stream\r\n\
              Content-Disposition: attachment; filename=\"a.bin\"\r\n\
              Content-Transfer-Encoding: base64\r\n\r\nAAAAAAAA\r\n--m--\r\n",
        ));
        assert!(body.plain().unwrap().contains("the body text"));
        // The attachment is binary, not an HTML part.
        assert_eq!(body.html(), None);
    }

    #[test]
    fn mixed_with_leading_text_keeps_real_html_not_a_synthesis() {
        // Regression: a leading text/plain before a multipart/alternative makes
        // mail-parser put that text part at html_body[0], so `body_html(0)` would
        // synthesize HTML from "LEADING_PLAIN" and drop the genuine `<p>REAL</p>`.
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nLEADING_PLAIN\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
              --a\r\nContent-Type: text/plain\r\n\r\nALT_PLAIN\r\n\
              --a\r\nContent-Type: text/html\r\n\r\n<p>REAL_HTML</p>\r\n--a--\r\n--m--\r\n",
        ));
        let html = body.html().expect("the real html part is captured");
        assert!(html.contains("REAL_HTML"), "{html}");
        assert!(
            !html.contains("LEADING_PLAIN"),
            "must not be a text->html synthesis: {html}"
        );
    }

    #[test]
    fn hostile_input_never_panics() {
        // Empty, raw garbage (incl. non-UTF-8 bytes), and a truncated multipart
        // whose boundary never closes — none may panic; an unparseable message is
        // an empty body.
        assert!(extract_body(&raw(b"")).is_empty());
        let _ = extract_body(&raw(b"\xff\xfe not a message {{{{ \x00\x01"));
        let _ = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"x\"\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nunterminated",
        ));
    }
}
