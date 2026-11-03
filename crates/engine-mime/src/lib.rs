//! `engine-mime` — MIME/RFC 5322 body extraction.
//!
//! One pure function, [`extract_body`], turns a message's raw RFC 5322 source
//! ([`RawMime`], the lossless Tier-3 blob the store caches on demand —
//! `north-star.md`) into a displayable [`MessageBody`](engine_core::mail::MessageBody): the best
//! `text/plain` and `text/html` bodies, content-transfer- and charset-decoded to UTF-8.
//!
//! The decoding is delegated to [`mail_parser`] — a hardened, pure-Rust parser
//! (`Cargo.toml` records why we depend on it rather than hand-rolling MIME). Mail
//! is hostile input: malformed or truncated bytes yield an **empty** body, never a
//! panic (`north-star.md` security). The crate is I/O-free and async-free, so the
//! provider/store layers own *fetching* and *caching* the raw bytes and this layer
//! only *interprets* them.
//!
//! [`encoded_word`] is the same decoding one level up, for RFC 2047 header text. It is
//! here rather than in an adapter because two adapters read RFC 5322 headers themselves
//! (IMAP's `ENVELOPE`, Gmail's `payload.headers`) and must agree with each other, and
//! with a body, about what a charset label means.

mod attachment;
mod body;
pub mod encoded_word;
mod scheduling;
mod structure;

pub use attachment::{extract_attachment, extract_attachments, has_downloadable_attachment};
pub use body::extract_body;
use engine_core::{mail::InlinePart, raw::RawMime};
use mail_parser::{ContentType, MessageParser, PartKind};
// The mail side of the mail↔calendar bridge: the iMIP `text/calendar` payload, and the
// addresses the message was delivered to (which is how an invitation to an alias is
// recognized with no configuration).
pub use scheduling::{CalendarPart, extract_calendar_part, extract_delivery_recipients};
pub use structure::is_single_text_body;

/// Renders `html` as text: tags removed, entities decoded, white space collapsed, and what
/// `<head>`, `<style>`, `<script>` and `<template>` hold left out, whatever attributes the
/// tag carries. It is the rendering [`extract_body`] gives an HTML part with no text version,
/// for HTML that arrives outside a message (an event's conference notes).
#[must_use]
pub fn html_to_text(html: &str) -> String {
    mail_parser::decoders::html::html_to_text(html)
}

/// Extracts a message's inline (`cid:`-referenced) parts — the decoded bytes a host
/// inlines for an `<img src="cid:…">` in the HTML body.
///
/// Returns one [`InlinePart`] per **binary** leaf part of the message itself (not of a
/// message it forwards) that declares a `Content-ID` (the only parts a `cid:` URL can
/// address, RFC 2392): the id with angle brackets stripped, the `Content-Type` media type
/// (parameters stripped), and the content-transfer-decoded bytes. Parts without a
/// `Content-ID`, and `text/*`/container parts, are skipped. Whether to actually inline a
/// part — and which media types are safe to render — is the **host's** policy, not this
/// function's (the bytes are hostile input).
///
/// Like [`extract_body`], a message that cannot be parsed, or that carries no such part,
/// yields an empty `Vec`, never a panic.
#[must_use]
pub fn extract_inline_parts(raw: &RawMime) -> Vec<InlinePart> {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return Vec::new();
    };

    message
        .root()
        .parts()
        .filter_map(|part| {
            // Only binary leaf parts hold inline attachment bytes; text and multipart parts
            // are never `cid:` image targets.
            if !matches!(part.kind(), PartKind::Binary | PartKind::InlineBinary) {
                return None;
            }
            // A part is addressable by `cid:` only if it carries a Content-ID.
            let content_id = part.content_id().and_then(content_id_token)?;
            let media_type = part
                .content_type()
                .map_or_else(|| "application/octet-stream".to_owned(), media_type_of);
            Some(InlinePart::new(
                content_id,
                media_type,
                part.decoded().into_owned(),
            ))
        })
        .collect()
}

/// The bare `Content-ID` token a `cid:` URL references — the header value with any
/// surrounding angle brackets and whitespace removed (mail-parser already drops the
/// brackets of a well-formed id; this is belt-and-braces). `None` for a blank value.
fn content_id_token(raw: &str) -> Option<String> {
    let token = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The media type (`type/subtype`, parameters stripped) of a parsed `Content-Type`, or
/// just the type when no subtype is present.
fn media_type_of(content_type: ContentType<'_>) -> String {
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
    fn an_html_only_message_leaves_its_stylesheet_out_of_the_text() {
        let body = extract_body(&raw(b"Content-Type: text/html; charset=utf-8\r\n\r\n\
              <style type=\"text/css\">body, p, div {font:12px Verdana;}</style>\
              <p>Dear reader,</p>\r\n"));
        let plain = body.plain().unwrap();
        assert!(!plain.contains("Verdana"), "{plain}");
        assert_eq!(plain.trim(), "Dear reader,");
    }

    #[test]
    fn every_non_text_element_is_left_out_whatever_its_attributes() {
        // mail-parser 0.11 recognised only the bare tags, so `<style type="text/css">` put a
        // stylesheet into the snippet and the search index; 1.0's converter handles both.
        let body = extract_body(&raw(b"Content-Type: text/html\r\n\r\n\
              <HEAD lang=\"nl\"><meta charset=\"utf-8\"></HEAD>\
              <Style media=\"screen\">.a{color:red}</Style>\
              <script type=\"text/javascript\">track()</script>\
              <template id=\"t\">hidden</template><styles>s</styles><p>shown</p>\r\n"));
        assert_eq!(body.plain().unwrap().trim(), "sshown");
    }

    #[test]
    fn the_html_says_it_is_utf8_once_decoded() {
        // The HTML is handed over as UTF-8 text; a `<meta>` still naming the source charset
        // would have a browser decode it a second time.
        let body = extract_body(&raw(
            b"Content-Type: text/html; charset=iso-8859-1\r\n\r\n\
              <html><head><meta charset=\"iso-8859-1\"></head><body>caf\xe9</body></html>\r\n",
        ));
        let html = body.html().unwrap();
        assert!(html.contains("<meta charset=\"utf-8\">"), "{html}");
        assert!(html.contains("café"), "{html}");
    }

    #[test]
    fn a_named_html_alternative_is_the_html_body_not_a_file() {
        // RFC 8621 makes a named, non-first alternative an attachment and copies the
        // `text/plain` in as the HTML stand-in; the sender's HTML is the body all the same.
        let raw = raw(
            b"Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
              --a\r\nContent-Type: text/plain\r\n\r\nplain\r\n\
              --a\r\nContent-Type: text/html; name=\"message.html\"\r\n\r\n<p>REAL</p>\r\n\
              --a--\r\n",
        );
        let body = extract_body(&raw);
        assert!(body.html().unwrap().contains("<p>REAL</p>"));
        assert_eq!(body.plain().unwrap().trim(), "plain");
        assert_eq!(extract_attachments(&raw), []);
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
    fn mixed_with_leading_text_keeps_real_html_and_the_leading_text() {
        // A leading text/plain before a multipart/alternative puts that text part at
        // html_body[0]. `body_html(0)` would hand back HTML synthesized from "LEADING_PLAIN"
        // and drop the genuine `<p>REAL_HTML</p>`; the HTML is both, in reading order.
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nLEADING_PLAIN\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
              --a\r\nContent-Type: text/plain\r\n\r\nALT_PLAIN\r\n\
              --a\r\nContent-Type: text/html\r\n\r\n<p>REAL_HTML</p>\r\n--a--\r\n--m--\r\n",
        ));
        let html = body.html().expect("the real html part is captured");
        let (leading, real) = (html.find("LEADING_PLAIN"), html.find("<p>REAL_HTML</p>"));
        assert!(
            leading.is_some() && real.is_some() && leading < real,
            "{html}"
        );
        assert!(
            !html.contains("ALT_PLAIN"),
            "the text version is not shown twice: {html}"
        );
        assert_eq!(body.plain().unwrap(), "LEADING_PLAIN\n\nALT_PLAIN");
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
        assert_eq!(
            extract_inline_parts(&raw(b"Content-Type: text/plain\r\n\r\njust text")),
            []
        );
        assert_eq!(
            extract_inline_parts(&raw(b"Content-Type: text/html\r\n\r\n<p>hi</p>")),
            []
        );
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
        assert_eq!(extract_inline_parts(&raw(b"")), []);
        let _ = extract_inline_parts(&raw(b"\xff\xfe \x00 not a message <<< cid:\x01"));
        let _ = extract_inline_parts(&raw(
            b"Content-Type: multipart/related; boundary=\"x\"\r\n\r\n--x\r\n\
              Content-Type: image/png\r\nContent-ID: <unterminated",
        ));
    }
}

/// The longest list snippet kept for a message.
///
/// [`Message::preview`](engine_core::mail::Message::preview) is documented as ≤256 chars and a
/// list row shows about two lines, so a little under that is plenty and keeps the store lean.
pub const PREVIEW_MAX_CHARS: usize = 200;

/// Derives a compact, whitespace-collapsed list snippet from an extracted body.
///
/// **The one place this rule lives.** IMAP has no server snippet, so one is derived from the
/// body — and the body arrives by two different roads (the adapter reading it during a delta
/// page, the engine caching it for offline reading), which would otherwise each grow their own
/// idea of what a snippet is and disagree on the same message.
///
/// `None` when the message carries no usable text: a row with nothing to say degrades to
/// sender-and-subject rather than showing noise.
#[must_use]
pub fn preview_from_body(body: &engine_core::mail::MessageBody) -> Option<String> {
    // Built up to the cap and stopped there, rather than collapsing the whole body and cutting
    // the result down. A newsletter's text part runs to tens of kilobytes and this keeps 200
    // characters of it, so the obvious `split_whitespace().collect().join(" ")` allocates the
    // whole body twice over to discard nearly all of it — for every message whose body is read.
    let mut preview = String::with_capacity(PREVIEW_MAX_CHARS);
    let mut chars = 0usize;
    for word in body.plain()?.split_whitespace() {
        if chars == PREVIEW_MAX_CHARS {
            break;
        }
        if chars > 0 {
            preview.push(' ');
            chars += 1;
        }
        for ch in word.chars() {
            if chars == PREVIEW_MAX_CHARS {
                break;
            }
            preview.push(ch);
            chars += 1;
        }
    }
    (!preview.is_empty()).then_some(preview)
}

#[cfg(test)]
mod preview_tests {
    use engine_core::mail::MessageBody;

    use super::{PREVIEW_MAX_CHARS, preview_from_body};

    #[test]
    fn a_snippet_collapses_whitespace_and_stops_on_a_char_boundary() {
        let body = MessageBody::new(
            Some(format!("  line one\n\n  line   two  {}", "é".repeat(400))),
            None,
        );
        let preview = preview_from_body(&body).expect("text");
        assert!(preview.starts_with("line one line two"));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn a_long_body_is_not_collapsed_whole_just_to_keep_its_first_line() {
        // The regression this guards is invisible in the output and only shows in the cost: a
        // body that is collapsed and *then* cut allocates all of itself to keep 200 chars, on
        // every message whose body is read. Asserted through the one observable consequence —
        // the result is capped and the words are in order — plus a body far larger than the cap.
        let word = "alpha ";
        let body = MessageBody::new(Some(word.repeat(50_000)), None);
        let preview = preview_from_body(&body).expect("text");
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
        assert!(preview.starts_with("alpha alpha"));
        assert!(
            !preview.ends_with(' '),
            "no trailing separator: {preview:?}"
        );
    }

    #[test]
    fn a_body_with_no_text_has_no_snippet() {
        assert!(preview_from_body(&MessageBody::new(Some(String::new()), None)).is_none());
        assert!(preview_from_body(&MessageBody::new(Some("   \n\t ".to_owned()), None)).is_none());
    }
}
