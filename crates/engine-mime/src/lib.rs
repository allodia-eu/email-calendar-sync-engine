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

use engine_core::mail::{InlinePart, MessageBody};
use engine_core::raw::RawMime;
use mail_parser::{ContentType, GetHeader, HeaderName, HeaderValue, MessageParser, PartType};

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

/// Extracts a message's inline (`cid:`-referenced) parts — the decoded bytes a host
/// inlines for an `<img src="cid:…">` in the HTML body.
///
/// Returns one [`InlinePart`] per **binary** leaf part that declares a `Content-ID`
/// (the only parts a `cid:` URL can address, RFC 2392): the id with angle brackets
/// stripped, the `Content-Type` media type (parameters stripped), and the
/// content-transfer-decoded bytes. Parts without a `Content-ID`, and `text/*`/container
/// parts, are skipped. Whether to actually inline a part — and which media types are
/// safe to render — is the **host's** policy, not this function's (the bytes are hostile
/// input).
///
/// Like [`extract_body`], a message that cannot be parsed, or that carries no such part,
/// yields an empty `Vec`, never a panic.
#[must_use]
pub fn extract_inline_parts(raw: &RawMime) -> Vec<InlinePart> {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return Vec::new();
    };

    message
        .parts
        .iter()
        .filter_map(|part| {
            // Only binary leaf parts hold inline attachment bytes (mail-parser has already
            // content-transfer-decoded them); text and multipart parts are never `cid:`
            // image targets.
            let bytes = match &part.body {
                PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.as_ref(),
                _ => return None,
            };
            // A part is addressable by `cid:` only if it carries a Content-ID.
            let content_id = part
                .headers
                .header_value(&HeaderName::ContentId)
                .and_then(content_id_token)?;
            let media_type = part
                .headers
                .header_value(&HeaderName::ContentType)
                .and_then(HeaderValue::as_content_type)
                .map_or_else(|| "application/octet-stream".to_owned(), media_type_of);
            Some(InlinePart::new(content_id, media_type, bytes.to_vec()))
        })
        .collect()
}

/// The bare `Content-ID` token a `cid:` URL references — the header value with any
/// surrounding angle brackets and whitespace removed (mail-parser parses Content-ID as a
/// message-id, so brackets are usually already gone; this is belt-and-braces). `None` for
/// a blank or non-text value.
fn content_id_token(value: &HeaderValue) -> Option<String> {
    let raw = match value {
        HeaderValue::Text(text) => text.as_ref(),
        HeaderValue::TextList(list) => list.first()?.as_ref(),
        _ => return None,
    };
    let token = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The media type (`type/subtype`, parameters stripped) of a parsed `Content-Type`, or
/// just the type when no subtype is present.
fn media_type_of(content_type: &ContentType) -> String {
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_owned(),
    }
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

    // A `multipart/related` whose HTML references an inline image by `cid:`, the image
    // carried in a sibling part with a matching `Content-ID`. `aGVsbG8=` is base64 for
    // `hello`, so the decoded bytes are easy to assert.
    const RELATED_WITH_INLINE_IMAGE: &[u8] =
        b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
        --b\r\nContent-Type: text/html\r\n\r\n<p><img src=\"cid:logo@allodia\"></p>\r\n\
        --b\r\nContent-Type: image/png\r\nContent-ID: <logo@allodia>\r\n\
        Content-Transfer-Encoding: base64\r\nContent-Disposition: inline\r\n\r\naGVsbG8=\r\n\
        --b--\r\n";

    #[test]
    fn extracts_inline_image_with_decoded_bytes_and_stripped_cid() {
        let parts = extract_inline_parts(&raw(RELATED_WITH_INLINE_IMAGE));
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        // Angle brackets are stripped: `<logo@allodia>` is referenced as `cid:logo@allodia`.
        assert_eq!(part.content_id(), "logo@allodia");
        assert_eq!(part.media_type(), "image/png");
        // Content-transfer-decoded (base64 `aGVsbG8=` -> `hello`).
        assert_eq!(part.bytes(), b"hello");
    }

    #[test]
    fn ignores_parts_without_a_content_id() {
        // A regular (non-inline) attachment has no Content-ID, so it is not `cid:`-addressable
        // and must not appear among the inline parts.
        let parts = extract_inline_parts(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
              --m\r\nContent-Type: application/pdf\r\n\
              Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--m--\r\n",
        ));
        assert!(parts.is_empty(), "{parts:?}");
    }

    #[test]
    fn plain_and_html_only_messages_have_no_inline_parts() {
        assert!(
            extract_inline_parts(&raw(b"Content-Type: text/plain\r\n\r\njust text")).is_empty()
        );
        assert!(extract_inline_parts(&raw(b"Content-Type: text/html\r\n\r\n<p>hi</p>")).is_empty());
    }

    #[test]
    fn extracts_every_inline_part_in_order() {
        let parts = extract_inline_parts(&raw(
            b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:a\"><img src=\"cid:b\">\r\n\
              --b\r\nContent-Type: image/gif\r\nContent-ID: <a>\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --b\r\nContent-Type: image/jpeg\r\nContent-ID: <b>\r\n\
              Content-Transfer-Encoding: base64\r\n\r\nd29ybGQ=\r\n\
              --b--\r\n",
        ));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].content_id(), "a");
        assert_eq!(parts[0].media_type(), "image/gif");
        assert_eq!(parts[0].bytes(), b"hello");
        assert_eq!(parts[1].content_id(), "b");
        assert_eq!(parts[1].media_type(), "image/jpeg");
        assert_eq!(parts[1].bytes(), b"world"); // base64 `d29ybGQ=`
    }

    #[test]
    fn inline_extraction_never_panics_on_hostile_input() {
        // Same adversarial posture as body extraction: garbage, non-UTF-8, and a
        // never-closed multipart boundary yield an empty list, never a panic.
        assert!(extract_inline_parts(&raw(b"")).is_empty());
        let _ = extract_inline_parts(&raw(b"\xff\xfe \x00 not a message <<< cid:\x01"));
        let _ = extract_inline_parts(&raw(
            b"Content-Type: multipart/related; boundary=\"x\"\r\n\r\n--x\r\n\
              Content-Type: image/png\r\nContent-ID: <unterminated",
        ));
    }
}
