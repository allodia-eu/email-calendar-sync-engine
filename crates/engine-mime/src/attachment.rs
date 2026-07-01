//! Attachment extraction from cached raw MIME.

use engine_core::mail::{AttachmentPartId, MessageAttachment, MessageAttachmentContent};
use engine_core::raw::RawMime;
use mail_parser::{ContentType, MessageParser, MessagePart, MimeHeaders, PartType};

/// Extracts downloadable attachment metadata from a raw RFC 5322 message.
///
/// The returned ids are message-scoped parser attachment indexes. Inline CID image parts are
/// deliberately omitted from this list because the reading view resolves them through
/// `extract_inline_parts`; listing them again would make embedded body images look like
/// downloadable file attachments. Other inline parts, such as a provider-displayed PDF with
/// no `Content-ID`, remain downloadable.
#[must_use]
pub fn extract_attachments(raw: &RawMime) -> Vec<MessageAttachment> {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return Vec::new();
    };

    message
        .attachments()
        .enumerate()
        .filter_map(|(index, part)| {
            let id = AttachmentPartId::new(u32::try_from(index).ok()?);
            attachment_meta(id, part)
        })
        .collect()
}

/// Extracts one downloadable attachment and its decoded bytes from a raw RFC 5322 message.
///
/// Returns `None` when `id` does not exist or points to an inline CID part that is handled by
/// `extract_inline_parts` instead.
#[must_use]
pub fn extract_attachment(raw: &RawMime, id: AttachmentPartId) -> Option<MessageAttachmentContent> {
    let message = MessageParser::default().parse(raw.as_bytes())?;
    let part = message.attachment(id.as_u32())?;
    let meta = attachment_meta(id, part)?;
    Some(MessageAttachmentContent::new(
        meta,
        part.contents().to_vec(),
    ))
}

fn attachment_meta(id: AttachmentPartId, part: &MessagePart<'_>) -> Option<MessageAttachment> {
    if is_cid_inline(part) || matches!(part.body, PartType::Multipart(_)) {
        return None;
    }
    let media_type = part
        .content_type()
        .map_or_else(|| "application/octet-stream".to_owned(), media_type_of);
    let file_name = part
        .attachment_name()
        .filter(|name| !name.trim().is_empty())
        .map(safe_file_name)
        .unwrap_or_else(|| default_file_name(id, &media_type));
    let inline = part
        .content_disposition()
        .is_some_and(ContentType::is_inline);
    let content_id = part.content_id().map(content_id_token);
    Some(MessageAttachment::new(
        id,
        file_name,
        media_type,
        part.contents().len() as u64,
        inline,
        content_id,
    ))
}

fn is_cid_inline(part: &MessagePart<'_>) -> bool {
    part.content_id().is_some()
        && (matches!(part.body, PartType::InlineBinary(_))
            || part
                .content_disposition()
                .is_some_and(ContentType::is_inline))
}

fn media_type_of(content_type: &ContentType<'_>) -> String {
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_owned(),
    }
}

fn content_id_token(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_owned()
}

fn default_file_name(id: AttachmentPartId, media_type: &str) -> String {
    let ext = match media_type {
        "message/rfc822" => "eml",
        "text/calendar" => "ics",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/pdf" => "pdf",
        media if media.starts_with("image/") => media.rsplit('/').next().unwrap_or("img"),
        _ => "bin",
    };
    format!("attachment-{}.{}", id.as_u32() + 1, safe_extension(ext))
}

fn safe_file_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "attachment.bin".to_owned()
    } else {
        out
    }
}

fn safe_extension(value: &str) -> String {
    let ext: String = value
        .chars()
        .take(16)
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if ext.is_empty() {
        "bin".to_owned()
    } else {
        ext
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bytes: &[u8]) -> RawMime {
        RawMime::new(bytes)
    }

    const MIXED_WITH_ATTACHMENTS: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
        --m\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
        Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
        Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
        --m\r\nContent-Type: text/calendar\r\n\
        Content-Disposition: attachment; filename=\"invite.ics\"\r\n\r\nBEGIN:VCALENDAR\r\n\
        --m--\r\n";

    #[test]
    fn extracts_attachment_metadata_without_bytes() {
        let attachments = extract_attachments(&raw(MIXED_WITH_ATTACHMENTS));
        assert_eq!(attachments.len(), 2, "{attachments:?}");
        assert_eq!(attachments[0].id().as_u32(), 0);
        assert_eq!(attachments[0].file_name(), "report.pdf");
        assert_eq!(attachments[0].media_type(), "application/pdf");
        assert_eq!(attachments[0].size(), 3);
        assert!(!attachments[0].is_inline());
        assert_eq!(attachments[1].file_name(), "invite.ics");
        assert_eq!(attachments[1].media_type(), "text/calendar");
    }

    #[test]
    fn extracts_selected_attachment_content() {
        let content = extract_attachment(&raw(MIXED_WITH_ATTACHMENTS), AttachmentPartId::new(0))
            .expect("attachment");
        assert_eq!(content.attachment().file_name(), "report.pdf");
        assert_eq!(content.bytes(), b"PDF");
        assert!(
            extract_attachment(&raw(MIXED_WITH_ATTACHMENTS), AttachmentPartId::new(9)).is_none()
        );
    }

    #[test]
    fn skips_cid_inline_images_but_keeps_inline_files_without_cid() {
        let raw = raw(b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
              --b\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
              Content-Disposition: inline; filename=\"logo.png\"\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --b\r\nContent-Type: application/pdf\r\n\
              Content-Disposition: inline; filename=\"preview.pdf\"\r\n\r\nPDF\r\n\
              --b--\r\n");
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(attachments[0].file_name(), "preview.pdf");
        assert!(attachments[0].is_inline());
    }

    #[test]
    fn sanitizes_suggested_file_names() {
        let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: application/octet-stream\r\n\
              Content-Disposition: attachment; filename=\"..\\secret/report?.bin\"\r\n\r\nbytes\r\n\
              --m--\r\n");
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments[0].file_name(), "..secret_report_.bin");
    }

    #[test]
    fn hostile_input_never_panics() {
        assert!(extract_attachments(&raw(b"")).is_empty());
        assert!(extract_attachment(&raw(b"\xff\x00not mime"), AttachmentPartId::new(0)).is_none());
    }
}
