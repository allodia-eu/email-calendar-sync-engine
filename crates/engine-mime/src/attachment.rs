//! Attachment extraction from cached raw MIME.

use engine_core::{
    mail::{AttachmentPartId, MessageAttachment, MessageAttachmentContent},
    raw::RawMime,
};
use mail_parser::{ContentType, Message, MessageParser, MessagePart, PartKind, PartRole};

use crate::body::is_alternative_version;

/// Extracts downloadable attachment metadata from a raw RFC 5322 message.
///
/// The returned ids are the parts' MIME part numbers: every part of the message, nested
/// messages included, numbered in document order from the root's `0`. They depend on the
/// structure alone, not on how a parser classifies the parts, so a listed id can only ever
/// name the part it was listed for.
///
/// The list is the message's own leaf parts (not those of a message it forwards) that are
/// either an attachment in the RFC 8621 sense or an image, audio or video part shown inside
/// the body, such as a photo an iPhone places between two paragraphs. The engine renders no
/// such part inline, so without it a photo with no `Content-ID` would be reachable nowhere.
///
/// A binary part carrying a `Content-ID` (an embedded `cid:` image or similar body resource)
/// is deliberately omitted because the reading view resolves it through
/// `extract_inline_parts`; listing it again would make an embedded body image look like a
/// downloadable file attachment. The one exception is a part the sender explicitly marked
/// `Content-Disposition: attachment`, which stays downloadable even when it also carries a
/// `Content-ID`. Parts without a `Content-ID`, such as a provider-displayed inline PDF, remain
/// downloadable. A `text/plain` or `text/html` version of a `multipart/alternative` is the
/// body, never a file, even when it carries a name.
///
/// An **iMIP `text/calendar` body part** — a meeting invitation's payload, which the reading
/// view renders as an invitation card — is likewise omitted. The rule is the absence of any
/// `Content-Disposition`, so a calendar file the sender explicitly *attached* (Gmail's
/// duplicate `invite.ics`, a published `.ics`) still appears; only the part consumed into the
/// card disappears.
#[must_use]
pub fn extract_attachments(raw: &RawMime) -> Vec<MessageAttachment> {
    let Some(message) = MessageParser::default().parse(raw.as_bytes()) else {
        return Vec::new();
    };

    downloadable_parts(&message)
        .enumerate()
        .map(|(ordinal, part)| attachment_meta(ordinal, part))
        .collect()
}

/// Extracts one downloadable attachment and its decoded bytes from a raw RFC 5322 message.
///
/// Returns `None` when `id` names no part, or a part that [`extract_attachments`] does not
/// list (see there for the exact rule).
#[must_use]
pub fn extract_attachment(raw: &RawMime, id: AttachmentPartId) -> Option<MessageAttachmentContent> {
    let message = MessageParser::default().parse(raw.as_bytes())?;
    let (ordinal, part) = downloadable_parts(&message)
        .enumerate()
        .find(|(_, part)| part.id() == id.as_u32())?;
    Some(MessageAttachmentContent::new(
        attachment_meta(ordinal, part),
        part.decoded().into_owned(),
    ))
}

/// Whether the message carries a download: [`extract_attachments`] would list at least one part.
///
/// The answer a list row's paperclip should give once the bytes are at hand, without building
/// any part's metadata. `None` for bytes that do not parse as a message: they say nothing about
/// it, so a caller keeps whatever it knew rather than recording "none" (an empty or truncated
/// source a provider returned must not clear a paperclip the server set correctly).
#[must_use]
pub fn has_downloadable_attachment(raw: &RawMime) -> Option<bool> {
    MessageParser::default()
        .parse(raw.as_bytes())
        .map(|message| downloadable_parts(&message).next().is_some())
}

/// The parts [`extract_attachments`] lists, in document order. Listing and fetching by id
/// both read this one sequence, so they cannot disagree about which parts are downloads.
fn downloadable_parts<'m>(message: &'m Message<'_>) -> impl Iterator<Item = MessagePart<'m>> {
    message.root().parts().filter(|part| {
        (part.is_attachment() || part.role() == PartRole::MediaBody)
            && !part.is_multipart()
            && !is_cid_inline(part)
            && !is_imip_body_part(part)
            && !is_alternative_version(part)
    })
}

/// The metadata of the `ordinal`-th downloadable `part`; the ordinal only numbers the
/// fallback file name, so the first unnamed attachment is `attachment-1`.
fn attachment_meta(ordinal: usize, part: MessagePart<'_>) -> MessageAttachment {
    let id = AttachmentPartId::new(part.id());
    let media_type = part
        .content_type()
        .map_or_else(|| "application/octet-stream".to_owned(), media_type_of);
    let file_name = part
        .attachment_name()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(|| default_file_name(ordinal, &media_type), safe_file_name);
    let inline = part
        .content_disposition()
        .is_some_and(|disposition| disposition.is_inline());
    let content_id = part.content_id().map(content_id_token);
    MessageAttachment::new(
        id,
        file_name,
        media_type,
        part.decoded_len() as u64,
        inline,
        content_id,
    )
}

/// Whether `part` is a `Content-ID` body resource the reading view resolves through
/// `extract_inline_parts`, so it must not also surface as a downloadable attachment.
///
/// It matches exactly what `extract_inline_parts` returns — a binary leaf with a `Content-ID`
/// — with one carve-out: a part explicitly marked `Content-Disposition: attachment` stays
/// downloadable even when it carries a `Content-ID`. Keying on `PartKind::Binary` **and**
/// `InlineBinary` matters because the kind follows the disposition: an embedded image of a
/// `multipart/mixed` with no disposition is `Binary`, and would otherwise be listed as a file
/// attachment (and double-listed with the inline parts).
fn is_cid_inline(part: &MessagePart<'_>) -> bool {
    part.content_id().is_some()
        && matches!(part.kind(), PartKind::Binary | PartKind::InlineBinary)
        && !part
            .content_disposition()
            .is_some_and(|disposition| disposition.is_attachment())
}

/// Whether `part` is an **iMIP scheduling body part** — the `text/calendar` payload of a
/// meeting invitation that a reading view renders as an *invitation card*, not as a file.
///
/// An iMIP message carries its iTIP object as an alternative **body** part, a sibling of
/// `text/plain` and `text/html` (RFC 6047 §2.4). It is a representation *of the message*, so
/// no paperclip belongs on it — Outlook and Gmail both hide it. RFC 8621 nonetheless
/// classifies it as an attachment (inside a `multipart/alternative` only `text/plain`,
/// `text/html` and inline media are body parts), so without this check every invitation
/// grew a junk `attachment-3.ics` row.
///
/// The discriminator is the **absence of any `Content-Disposition`**, which is exactly what
/// separates a body part from a file the sender meant to send:
///
/// - Outlook and Sabre/CalDAV send only the undispositioned body part → hidden, card only.
/// - Gmail sends the body part **and** a `Content-Disposition: attachment` copy named `invite.ics`
///   → the body part is hidden, the deliberate copy keeps its chip (Outlook shows it too).
/// - A published `.ics` (`METHOD:PUBLISH`, a newsletter-style calendar file) is dispositioned as an
///   attachment → keeps its chip, and produces no card anyway.
///
/// So a sender who explicitly attached a calendar file always keeps it; only the part we
/// consume into the card disappears.
fn is_imip_body_part(part: &MessagePart<'_>) -> bool {
    part.is_content_type("text", "calendar") && part.content_disposition().is_none()
}

fn media_type_of(content_type: ContentType<'_>) -> String {
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

fn default_file_name(ordinal: usize, media_type: &str) -> String {
    let ext = match media_type {
        "message/rfc822" => "eml",
        "text/calendar" => "ics",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/pdf" => "pdf",
        media if media.starts_with("image/") => media.rsplit('/').next().unwrap_or("img"),
        _ => "bin",
    };
    format!("attachment-{}.{}", ordinal + 1, safe_extension(ext))
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
#[path = "attachment_tests.rs"]
mod tests;
