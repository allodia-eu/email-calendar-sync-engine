//! MIME body assembly for SMTP submission.

use std::fmt::Write as _;

use engine_provider::{Draft, DraftAttachment, DraftAttachmentDisposition};

use crate::error::{ImapError, ImapResult};

/// The body-specific headers and bytes appended after the RFC 5322 envelope
/// headers.
pub(super) struct MimeBody {
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
pub(super) fn assemble(draft: &Draft) -> ImapResult<MimeBody> {
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

fn body_part(draft: &Draft) -> Part {
    match &draft.html_body {
        Some(html) => multipart(
            "alternative",
            &boundary(draft, "alternative"),
            vec![
                text_part("plain", &draft.text_body),
                text_part("html", html),
            ],
        ),
        None => text_part("plain", &draft.text_body),
    }
}

fn text_part(kind: &str, body: &str) -> Part {
    let subtype = if kind == "html" { "html" } else { "plain" };
    let mut bytes = Vec::new();
    for line in super::normalize_body_lines(body) {
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    Part {
        content_headers: format!("Content-Type: text/{subtype}; charset=utf-8\r\n"),
        body: bytes,
    }
}

fn attachment_part(attachment: &DraftAttachment) -> ImapResult<Part> {
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
                super::reject_control("Content-ID", content_id.as_str())?
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
    let encoded = crate::base64::encode(content);
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

fn media_type(value: &str) -> ImapResult<&str> {
    let value = super::reject_control("attachment media type", value)?;
    if value.is_empty()
        || !value.bytes().all(
            |b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'+' | b'-' | b'.'),
        )
    {
        return Err(ImapError::protocol(
            "attachment media type is not safe for a MIME header",
        ));
    }
    Ok(value)
}

fn parameter(name: &str, value: &str) -> ImapResult<String> {
    let value = super::reject_control("attachment filename", value)?;
    if super::is_ascii_printable(value) {
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
