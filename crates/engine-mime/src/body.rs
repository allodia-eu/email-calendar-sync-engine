//! The displayable body of a message: its text and its HTML.

use std::borrow::Cow;

use engine_core::{mail::MessageBody, raw::RawMime};
use mail_parser::{Message, MessageParser, MessagePart, PartKind, decoders::html::text_to_html};

/// What stands between two text parts of one body, such as the paragraphs on either side of
/// a photo: a blank line, in place of the part the text cannot show.
const TEXT_PART_SEPARATOR: &str = "\n\n";

/// Extracts the displayable [`MessageBody`] from a raw RFC 5322 message.
///
/// A body is a list of parts read in order (RFC 8621 `textBody` and `htmlBody`): a message
/// from an iPhone places its text on either side of a photo, a mailing list appends its
/// footer as a part of its own. Each of the two renderings is that whole list, the parts it
/// cannot show (the photo) left out.
///
/// [`MessageBody::plain`] is the message's canonical text rendering — the decoded
/// `text/plain` parts, an HTML part converted where it has no text version — so a
/// plain-text reading view always has something to show. [`MessageBody::html`] is captured
/// only when the message carries a real `text/html` part (mail-parser copies a text-only
/// message's text into its HTML body list too, so the list alone does not prove one); a text
/// part of the list is then converted to HTML in its place. The HTML is **unsanitized** and
/// a host must sanitize before rendering. Its `<meta>` charset declarations are rewritten to
/// UTF-8, which is what the text now is.
///
/// A message that cannot be parsed, or carries no text part, yields
/// [`MessageBody::empty`].
#[must_use]
pub fn extract_body(raw: &RawMime) -> MessageBody {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return MessageBody::empty();
    };

    // HTML parts converted leave out the content of `<head>`, `<style>`, `<script>` and
    // `<template>`, whatever attributes the tag carries.
    let plain = join_text(message.text_bodies().collect());
    let html = join_html(&message);

    MessageBody::new(plain, html)
}

/// The HTML body list in reading order, `None` unless a real `text/html` part is in it.
///
/// A text part stands in for an alternative whose HTML version RFC 8621 left out because it
/// carries a name; that named version is the HTML the sender wrote, so it takes the stand-in's
/// place. Any other text part is converted to HTML in its place.
fn join_html(message: &Message<'_>) -> Option<String> {
    let named: Vec<MessagePart<'_>> = message
        .root()
        .parts()
        .filter(|part| matches!(part.kind(), PartKind::Html) && is_alternative_version(part))
        .collect();
    let version_of = |stand_in: &MessagePart<'_>| {
        let parent = stand_in.parent().map(|parent| parent.id());
        named
            .iter()
            .find(|part| part.parent().map(|parent| parent.id()) == parent)
            .copied()
    };
    let mut html = String::new();
    let mut real = false;
    for part in message.html_body() {
        let version = match part.kind() {
            PartKind::Html => Some(part),
            PartKind::Text => version_of(&part),
            _ => continue,
        };
        match version.and_then(|version| version.html_utf8()) {
            Some(text) => {
                real = true;
                html.push_str(&text);
            }
            None => {
                if let Some(text) = part.text() {
                    html.push_str(&text_to_html(&text));
                }
            }
        }
    }
    if real {
        Some(html)
    } else {
        named
            .first()
            .and_then(MessagePart::html_utf8)
            .map(Cow::into_owned)
    }
}

/// The texts of a body's parts as one text; a single part is kept exactly as written, and a
/// blank part between others adds no second gap.
fn join_text(mut texts: Vec<Cow<'_, str>>) -> Option<String> {
    if texts.len() <= 1 {
        return texts.pop().map(Cow::into_owned);
    }
    let mut joined = String::new();
    for text in texts.iter().filter(|text| !text.trim().is_empty()) {
        if !joined.is_empty() {
            joined.push_str(TEXT_PART_SEPARATOR);
        }
        joined.push_str(text.trim_end_matches(['\r', '\n']));
    }
    Some(joined)
}

/// Whether `part` is a `text/plain` or `text/html` version of a `multipart/alternative` that
/// RFC 8621 nonetheless leaves out of the body lists, because it carries a name.
///
/// The children of an alternative are versions of one body by definition (RFC 2046 §5.1.4),
/// so a `text/html; name="message.html"` there is the body the sender wrote, not a file:
/// read as the HTML body (in place of mail-parser's stand-in, the `text/plain` copied into
/// the HTML list) and never listed as a download, which is how IMAP's list-time
/// `has_attachment` counts it too.
pub(crate) fn is_alternative_version(part: &MessagePart<'_>) -> bool {
    (part.is_content_type("text", "plain") || part.is_content_type("text", "html"))
        && !part.in_text_body()
        && !part.in_html_body()
        && !part
            .content_disposition()
            .is_some_and(|disposition| disposition.is_attachment())
        && part
            .parent()
            .is_some_and(|parent| parent.is_content_type("multipart", "alternative"))
}

#[cfg(test)]
mod tests {
    use engine_core::raw::RawMime;

    use super::extract_body;

    fn raw(bytes: &[u8]) -> RawMime {
        RawMime::new(bytes)
    }

    #[test]
    fn the_text_on_both_sides_of_a_photo_is_one_text() {
        // An iPhone's plain-text message: text, the photo, more text, in one
        // `multipart/mixed`. Only the first part used to survive.
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nbefore the photo\r\n\
              --m\r\nContent-Type: image/jpeg; name=\"IMG_1.jpg\"\r\n\
              Content-Disposition: inline; filename=\"IMG_1.jpg\"\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nafter the photo\r\n--m--\r\n",
        ));
        assert_eq!(body.plain().unwrap(), "before the photo\n\nafter the photo");
        assert_eq!(body.html(), None, "there is no real HTML part");
    }

    #[test]
    fn every_html_part_of_the_body_is_shown_in_order() {
        // Apple Mail with formatting: an alternative, the photo, another alternative, then a
        // mailing list's plain footer. The footer is converted to HTML in its place.
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
              --a\r\nContent-Type: text/plain\r\n\r\none\r\n\
              --a\r\nContent-Type: text/html\r\n\r\n<p>ONE</p>\r\n--a--\r\n\
              --m\r\nContent-Type: image/jpeg\r\nContent-Disposition: inline\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\ntwo\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<p>TWO</p>\r\n--b--\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nfooter & more\r\n--m--\r\n",
        ));
        let html = body.html().unwrap();
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {html}"))
        };
        assert!(at("<p>ONE</p>") < at("<p>TWO</p>"));
        assert!(at("<p>TWO</p>") < at("footer &amp; more"));
        assert_eq!(body.plain().unwrap(), "one\n\ntwo\n\nfooter & more");
    }

    #[test]
    fn a_named_html_alternative_replaces_its_stand_in_beside_other_html() {
        // The first alternative's HTML carries a name, so RFC 8621 leaves it out of the body
        // lists and copies its text version in; the second alternative's HTML is real.
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
              --a\r\nContent-Type: text/plain\r\n\r\none\r\n\
              --a\r\nContent-Type: text/html; name=\"m.html\"\r\n\r\n<p>ONE</p>\r\n--a--\r\n\
              --m\r\nContent-Type: image/jpeg\r\nContent-Disposition: inline\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --m\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\ntwo\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<p>TWO</p>\r\n--b--\r\n--m--\r\n",
        ));
        let html = body.html().unwrap();
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {html}"))
        };
        assert!(at("<p>ONE</p>") < at("<p>TWO</p>"));
        assert!(
            !html.contains("one<br"),
            "the stand-in is not shown: {html}"
        );
    }

    #[test]
    fn a_blank_part_between_two_adds_no_second_gap() {
        let body = extract_body(&raw(
            b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\na\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nb\r\n--m--\r\n",
        ));
        assert_eq!(body.plain().unwrap(), "a\n\nb");
    }

    #[test]
    fn a_single_part_is_kept_exactly_as_written() {
        let body = extract_body(&raw(b"Content-Type: text/plain\r\n\r\nline\r\n\r\n"));
        assert_eq!(body.plain().unwrap(), "line\r\n\r\n");
    }
}
