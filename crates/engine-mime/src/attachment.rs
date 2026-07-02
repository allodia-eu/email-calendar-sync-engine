//! Attachment extraction from cached raw MIME.

use engine_core::mail::{AttachmentPartId, MessageAttachment, MessageAttachmentContent};
use engine_core::raw::RawMime;
use mail_parser::{ContentType, MessageParser, MessagePart, MimeHeaders, PartType};

/// Extracts downloadable attachment metadata from a raw RFC 5322 message.
///
/// The returned ids are message-scoped parser attachment indexes. A binary part carrying a
/// `Content-ID` (an embedded `cid:` image or similar body resource) is deliberately omitted
/// because the reading view resolves it through `extract_inline_parts`; listing it again would
/// make an embedded body image look like a downloadable file attachment. The one exception is
/// a part the sender explicitly marked `Content-Disposition: attachment`, which stays
/// downloadable even when it also carries a `Content-ID`. Parts without a `Content-ID`, such as
/// a provider-displayed inline PDF, remain downloadable.
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
/// Returns `None` when `id` does not exist or points to a `Content-ID` body part that is
/// handled by `extract_inline_parts` instead (see [`extract_attachments`] for the exact rule).
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
        .map_or_else(|| default_file_name(id, &media_type), safe_file_name);
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

/// Whether `part` is a `Content-ID` body resource the reading view resolves through
/// `extract_inline_parts`, so it must not also surface as a downloadable attachment.
///
/// It matches exactly what `extract_inline_parts` returns — a binary leaf with a `Content-ID`
/// — with one carve-out: a part explicitly marked `Content-Disposition: attachment` stays
/// downloadable even when it carries a `Content-ID`. Keying on `PartType::Binary` **and**
/// `InlineBinary` matters because mail-parser types a non-first `multipart/related` child image
/// as `Binary`, not `InlineBinary`, so an embedded `cid:` image with no explicit disposition
/// would otherwise be listed as a file attachment (and double-listed with the inline parts).
fn is_cid_inline(part: &MessagePart<'_>) -> bool {
    part.content_id().is_some()
        && matches!(part.body, PartType::Binary(_) | PartType::InlineBinary(_))
        && !part
            .content_disposition()
            .is_some_and(ContentType::is_attachment)
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
        if is_unsafe_name_char(ch) {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    // A name that is only dots (`.`, `..`) is a directory reference, not a file — a host doing
    // `dir.join(name)` would target a directory. An all-replaced or empty name has nothing
    // usable left. Fall back in both cases.
    if out.is_empty() || out.chars().all(|ch| ch == '.') {
        "attachment.bin".to_owned()
    } else {
        out
    }
}

/// Characters that must not survive into a suggested file name: path separators and
/// shell/Windows-reserved punctuation, ASCII/C1 control codes, and the Unicode bidirectional
/// and zero-width format controls (e.g. U+202E RIGHT-TO-LEFT OVERRIDE) used to disguise a
/// file's real extension in a host's UI.
fn is_unsafe_name_char(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        || matches!(ch,
            '\u{00AD}'                // soft hyphen
            | '\u{061C}'              // Arabic letter mark
            | '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM, RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings and overrides
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}',             // zero-width no-break space / BOM
        )
}

fn safe_extension(value: &str) -> String {
    let ext: String = value
        .chars()
        .take(16)
        .filter(char::is_ascii_alphanumeric)
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

    // A `multipart/related` embedded image referenced by `cid:` but carrying **no**
    // `Content-Disposition`. mail-parser types a non-first related child as `Binary` (not
    // `InlineBinary`), so this is the exact shape that used to leak into the attachment list.
    const RELATED_UNDISPOSED_CID: &[u8] =
        b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
        --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
        --b\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
        Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
        --b--\r\n";

    #[test]
    fn undisposed_related_cid_image_is_inline_not_a_downloadable_attachment() {
        // Regression: an embedded body image (cid:, no disposition) must not appear as a
        // downloadable file — and must not be double-listed with the inline parts.
        let attachments = extract_attachments(&raw(RELATED_UNDISPOSED_CID));
        assert!(attachments.is_empty(), "{attachments:?}");
        // It is still resolvable as an inline part for the HTML body.
        let inline = crate::extract_inline_parts(&raw(RELATED_UNDISPOSED_CID));
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].content_id(), "logo@x");
    }

    #[test]
    fn attachment_disposition_stays_downloadable_even_with_a_content_id() {
        // The sender explicitly marked this image `attachment`; a `Content-ID` on it must not
        // hide it from the download list.
        let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
              --m\r\nContent-Type: image/png\r\nContent-ID: <pic@x>\r\n\
              Content-Disposition: attachment; filename=\"pic.png\"\r\n\
              Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--m--\r\n");
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(attachments[0].file_name(), "pic.png");
        assert_eq!(attachments[0].content_id(), Some("pic@x"));
    }

    #[test]
    fn safe_file_name_replaces_bidi_and_zero_width_controls() {
        // U+202E RIGHT-TO-LEFT OVERRIDE is the classic extension-spoof; it must be neutralized,
        // along with the other bidi overrides, isolates, and zero-width/format controls.
        let safe = safe_file_name("invoice\u{202E}cod.exe");
        assert!(!safe.contains('\u{202E}'), "{safe:?}");
        assert_eq!(safe, "invoice_cod.exe");
        assert_eq!(safe_file_name("a\u{200B}b\u{FEFF}c"), "a_b_c");
        // A bidi isolate (U+2066) and the Arabic letter mark (U+061C) are equally spoofable.
        assert_eq!(safe_file_name("x\u{2066}y\u{061C}z"), "x_y_z");
    }

    #[test]
    fn safe_extension_falls_back_when_nothing_alphanumeric_remains() {
        assert_eq!(safe_extension("###"), "bin");
        assert_eq!(safe_extension("PdF"), "PdF");
    }

    #[test]
    fn safe_file_name_falls_back_on_dot_only_and_empty_names() {
        assert_eq!(safe_file_name(".."), "attachment.bin");
        assert_eq!(safe_file_name("."), "attachment.bin");
        assert_eq!(safe_file_name("   "), "attachment.bin");
        // A legitimate dotfile keeps its name.
        assert_eq!(safe_file_name(".gitignore"), ".gitignore");
    }

    #[test]
    fn unnamed_attachments_get_default_names_by_media_type() {
        // Parts with no filename anywhere fall back to a synthesized `attachment-N.ext` derived
        // from the media type (covers the pdf and image arms and the extension sanitizer).
        let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
              --m\r\nContent-Type: application/pdf\r\n\
              Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
              --m\r\nContent-Type: image/png\r\n\
              Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
              --m--\r\n");
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 2, "{attachments:?}");
        assert_eq!(attachments[0].file_name(), "attachment-1.pdf");
        assert_eq!(attachments[1].file_name(), "attachment-2.png");
    }

    #[test]
    fn attachment_with_a_bare_media_type_falls_back_to_octet_defaults() {
        // A Content-Type with a type but no subtype, and no filename: media type keeps the bare
        // type and the default name uses the `.bin` fallback extension.
        let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
              --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
              --m\r\nContent-Type: application\r\nContent-Disposition: attachment\r\n\r\nX\r\n\
              --m--\r\n");
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(attachments[0].media_type(), "application");
        assert_eq!(attachments[0].file_name(), "attachment-1.bin");
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
