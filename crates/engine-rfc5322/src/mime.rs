//! MIME body assembly: the `multipart/{alternative,related,mixed}` nesting under the
//! RFC 5322 envelope headers, and the base64 attachment parts.

use std::fmt::Write as _;

use engine_provider::{
    Draft, DraftAttachment, DraftAttachmentDisposition, DraftCalendar, ProviderError,
    ProviderResult,
};

use crate::{
    assemble::{is_ascii_printable, normalize_body_lines, reject_control},
    base64,
};

/// The body-specific headers and bytes appended after the RFC 5322 envelope headers.
pub(crate) struct MimeBody {
    /// Root MIME headers, terminated by CRLF but not by the blank header/body line.
    pub content_headers: String,
    /// Root MIME body bytes with CRLF line endings.
    pub body: Vec<u8>,
}

struct Part {
    content_headers: String,
    body: Vec<u8>,
}

/// Builds the root MIME body for a draft.
pub(crate) fn assemble(draft: &Draft) -> ProviderResult<MimeBody> {
    let inline = draft
        .attachments
        .iter()
        .filter(|part| part.is_inline())
        .collect::<Vec<_>>();
    let regular = draft
        .attachments
        .iter()
        .filter(|part| !part.is_inline())
        .collect::<Vec<_>>();

    let mut body = body_part(draft);
    if !inline.is_empty() {
        let mut parts = vec![body];
        for attachment in inline {
            parts.push(attachment_part(attachment)?);
        }
        body = multipart("related", &boundary(draft, "related"), parts);
    }
    if !regular.is_empty() {
        let mut parts = vec![body];
        for attachment in regular {
            parts.push(attachment_part(attachment)?);
        }
        body = multipart("mixed", &boundary(draft, "mixed"), parts);
    }

    Ok(MimeBody {
        content_headers: body.content_headers,
        body: body.body,
    })
}

/// The message's representations, wrapped in a `multipart/alternative` as soon as there is
/// more than one.
///
/// Ordered least-to-most faithful (RFC 2046 §5.1.4), so a receiving client picks the
/// richest form it understands: plain text, then the HTML alternative, then — most
/// faithful of all — the iTIP object, which *is* the message when the message is a
/// scheduling reply (RFC 6047 §2.4).
///
/// The iTIP object belongs here rather than among the attachments precisely because it is
/// a representation and not an enclosure: an attachment part carries a
/// `Content-Disposition` and no `method=` parameter, and a reply sent that way is filed as
/// a document instead of processed as an answer.
fn body_part(draft: &Draft) -> Part {
    let mut parts = vec![text_part("plain", &draft.text_body)];
    if let Some(html) = &draft.html_body {
        parts.push(text_part("html", html));
    }
    if let Some(calendar) = &draft.calendar {
        parts.push(calendar_part(calendar));
    }
    if parts.len() == 1 {
        return parts.remove(0);
    }
    multipart("alternative", &boundary(draft, "alternative"), parts)
}

/// The `text/calendar` body part of an iMIP message (RFC 6047 §2.4/§2.5).
///
/// Three things here are requirements rather than choices:
///
/// - **`method=`** — §2.4 requires it and receiving clients dispatch on it. Emitted in the
///   iCalendar `METHOD` property's own uppercase spelling (RFC 5545 §3.7.2), not the engine's
///   canonical lowercase one, so the two spellings a human sees in the raw message agree.
/// - **`charset=utf-8`** — §2.4: a `text/*` part defaults to US-ASCII, an iCalendar object to
///   UTF-8, so the parameter must be present or a non-ASCII `SUMMARY` is misdecoded.
/// - **base64, never `7bit`** — §2.5. iCalendar content lines are long and folded, and a transport
///   free to re-wrap them corrupts the object; base64 makes the bytes opaque. Line endings are
///   normalized to CRLF first (RFC 5545 §3.1), because base64 would otherwise carry a caller's bare
///   LF to the wire where a strict parser rejects it.
///
/// And one thing is deliberately absent: a `Content-Disposition`. This is not a file.
fn calendar_part(calendar: &DraftCalendar) -> Part {
    let mut lines = normalize_body_lines(&calendar.ical);
    // An object that already ended with a line break splits into a trailing empty segment;
    // emitting it would append a blank line *inside* the object. Harmless in a text body,
    // which is why `text_part` does not bother — but here the bytes are the payload, and
    // the part must decode to exactly the object the caller assembled.
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut body = Vec::new();
    for line in lines {
        body.extend_from_slice(line.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    let method = calendar.method.as_str().to_ascii_uppercase();
    Part {
        content_headers: format!(
            "Content-Type: text/calendar; charset=utf-8; method={method}\r\n\
             Content-Transfer-Encoding: base64\r\n"
        ),
        body: base64_body(&body),
    }
}

fn text_part(kind: &str, body: &str) -> Part {
    let subtype = if kind == "html" { "html" } else { "plain" };
    let mut bytes = Vec::new();
    for line in normalize_body_lines(body) {
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    Part {
        content_headers: format!("Content-Type: text/{subtype}; charset=utf-8\r\n"),
        body: bytes,
    }
}

fn attachment_part(attachment: &DraftAttachment) -> ProviderResult<Part> {
    let media_type = media_type(&attachment.media_type)?;
    let name = parameter("name", &attachment.file_name)?;
    let filename = parameter("filename", &attachment.file_name)?;
    let mut content_headers =
        format!("Content-Type: {media_type}; {name}\r\nContent-Transfer-Encoding: base64\r\n");
    match &attachment.disposition {
        DraftAttachmentDisposition::Inline { content_id } => {
            write!(
                &mut content_headers,
                "Content-ID: <{}>\r\nContent-Disposition: inline; {filename}\r\n",
                reject_control("Content-ID", content_id.as_str())?
            )
            .expect("writing to a String cannot fail");
        }
        DraftAttachmentDisposition::Attachment => {
            write!(
                &mut content_headers,
                "Content-Disposition: attachment; {filename}\r\n"
            )
            .expect("writing to a String cannot fail");
        }
    }
    Ok(Part {
        content_headers,
        body: base64_body(&attachment.content),
    })
}

fn multipart(subtype: &str, boundary: &str, parts: Vec<Part>) -> Part {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(part.content_headers.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.body);
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Part {
        content_headers: format!("Content-Type: multipart/{subtype}; boundary=\"{boundary}\"\r\n"),
        body,
    }
}

fn base64_body(content: &[u8]) -> Vec<u8> {
    let encoded = base64::encode(content);
    let mut body = Vec::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    for line in encoded.as_bytes().chunks(76) {
        body.extend_from_slice(line);
        body.extend_from_slice(b"\r\n");
    }
    body
}

fn boundary(draft: &Draft, kind: &str) -> String {
    let seed = draft
        .message_id
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(48)
        .collect::<String>();
    if seed.is_empty() {
        format!("=_pim_engine_{kind}")
    } else {
        format!("=_pim_engine_{kind}_{seed}")
    }
}

fn media_type(value: &str) -> ProviderResult<&str> {
    let value = reject_control("attachment media type", value)?;
    if value.is_empty()
        || !value.bytes().all(
            |b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'+' | b'-' | b'.'),
        )
    {
        return Err(ProviderError::permanent(
            "attachment media type is not safe for a MIME header",
        ));
    }
    Ok(value)
}

fn parameter(name: &str, value: &str) -> ProviderResult<String> {
    let value = reject_control("attachment filename", value)?;
    if is_ascii_printable(value) {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(format!("{name}=\"{escaped}\""))
    } else {
        Ok(format!(
            "{name}*=utf-8''{}",
            percent_encode(value.as_bytes())
        ))
    }
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out.push(char::from(byte));
        } else {
            write!(&mut out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}
