//! What a message's top-level headers alone say about its MIME structure.
//!
//! An adapter that reads a header block without the body (IMAP's
//! `BODY.PEEK[HEADER.FIELDS (…)]`) asks this before paying for the structure itself:
//! some servers build `BODYSTRUCTURE` by reading the whole message.

use mail_parser::{ContentType, HeaderName, MessageParser, MimeHeaders};

/// Whether the header block `headers` describes a message that is one text body and
/// nothing else: its top-level `Content-Type` is `text/plain` or `text/html`, and its
/// `Content-Disposition` is not `attachment`. Such a message has no part besides that
/// body, so it carries no download.
///
/// A missing `Content-Type` is `text/plain` (RFC 2045 §5.2). One that is present but
/// does not parse answers `false`, as does an unparseable block: the caller then
/// reads the structure rather than trusting a guess.
#[must_use]
pub fn is_single_text_body(headers: &[u8]) -> bool {
    let Some(message) = MessageParser::default().parse_headers(headers) else {
        // No header at all: the default type and no disposition.
        return headers.iter().all(u8::is_ascii_whitespace);
    };
    if message
        .content_disposition()
        .is_some_and(ContentType::is_attachment)
    {
        return false;
    }
    match message.content_type() {
        Some(content_type) => {
            content_type.ctype().eq_ignore_ascii_case("text")
                && content_type.subtype().is_some_and(|subtype| {
                    subtype.eq_ignore_ascii_case("plain") || subtype.eq_ignore_ascii_case("html")
                })
        }
        None => message.header(HeaderName::ContentType).is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_body_without_an_attachment_disposition_is_one_text_body() {
        for headers in [
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "Content-Type: TEXT/PLAIN\r\n\r\n",
            "References: <a@x>\r\nContent-Type: text/plain;\r\n charset=\"us-ascii\"\r\n\r\n",
            "Content-Type: text/plain; name=\"notes.txt\"\r\nContent-Disposition: inline\r\n\r\n",
            "References: <a@x>\r\n\r\n",
            "\r\n",
            "",
        ] {
            assert!(is_single_text_body(headers.as_bytes()), "{headers:?}");
        }
    }

    #[test]
    fn anything_that_may_hold_another_part_is_not() {
        for headers in [
            "Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n",
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n",
            "Content-Type: application/pdf; name=\"a.pdf\"\r\n\r\n",
            "Content-Type: text/calendar; method=REQUEST\r\n\r\n",
            "Content-Type: text/plain\r\nContent-Disposition: attachment; filename=a.txt\r\n\r\n",
            "Content-Disposition: ATTACHMENT\r\n\r\n",
            "Content-Type: text\r\n\r\n",
            "Content-Type: \r\n\r\n",
        ] {
            assert!(!is_single_text_body(headers.as_bytes()), "{headers:?}");
        }
    }
}
