//! The extracted, displayable body of a message.

use core::fmt;

use serde::{Deserialize, Serialize};

/// The text content extracted from a message's raw MIME source for display.
///
/// This is a *derived* view, not stored state: the lossless source is the cached
/// raw RFC 5322 ([`RawMime`](crate::raw::RawMime), Tier-3 content fetched on demand
/// — `north-star.md`), and this is what the MIME extractor decodes out of it (the
/// best `text/plain` and `text/html` parts, content-transfer- and charset-decoded
/// to UTF-8). A host renders [`plain`](Self::plain) for a plain-text reading view;
/// [`html`](Self::html) is captured for the later sanitized-HTML slice.
///
/// Either field is `None` when the message has no such part. Like the raw payloads,
/// its `Debug` is **redacted** — only the lengths print, never the content — because
/// body text is sensitive data and logs are redacted by default (`north-star.md`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    /// The decoded `text/plain` body, if the message has one (or a text rendering
    /// of an HTML-only message, when the extractor can derive it).
    plain: Option<String>,
    /// The decoded `text/html` body, if the message has one. Not yet sanitized;
    /// a host must sanitize before rendering (`north-star.md` security section).
    html: Option<String>,
}

impl MessageBody {
    /// Creates a body from its decoded plain-text and HTML parts.
    #[must_use]
    pub fn new(plain: Option<String>, html: Option<String>) -> Self {
        Self { plain, html }
    }

    /// Creates an empty body — the result when the source has no text part or
    /// could not be parsed.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plain: None,
            html: None,
        }
    }

    /// The decoded `text/plain` body, if any.
    #[must_use]
    pub fn plain(&self) -> Option<&str> {
        self.plain.as_deref()
    }

    /// The decoded, **unsanitized** `text/html` body, if any.
    #[must_use]
    pub fn html(&self) -> Option<&str> {
        self.html.as_deref()
    }

    /// Returns `true` if there is no non-empty text to display in either part —
    /// a present-but-empty part counts as empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plain.as_deref().unwrap_or_default().is_empty()
            && self.html.as_deref().unwrap_or_default().is_empty()
    }
}

impl fmt::Debug for MessageBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageBody")
            .field("plain_len", &self.plain.as_deref().map(str::len))
            .field("html_len", &self.html.as_deref().map(str::len))
            .finish_non_exhaustive()
    }
}

/// A message's inline (`cid:`-referenced) MIME part — the decoded bytes a host needs to
/// render an `<img src="cid:…">` in the HTML body, keyed by the `Content-ID` the
/// reference points at.
///
/// Like [`MessageBody`] this is a *derived* view of the raw RFC 5322 source, not stored
/// state: `engine-mime` decodes it out of the cached raw ([`RawMime`](crate::raw::RawMime))
/// on demand. [`bytes`](Self::bytes) is content-transfer-decoded (base64/quoted-printable
/// already undone). Its `Debug` is **redacted** — only the id, media type, and byte length
/// print, never the bytes — because inline content is as sensitive as the body text
/// (`north-star.md`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlinePart {
    /// The `Content-ID` with its surrounding angle brackets removed — the exact token a
    /// `cid:` URL addresses (RFC 2392): a part `Content-ID: <logo@x>` is referenced by
    /// `cid:logo@x`.
    content_id: String,
    /// The media type with parameters stripped (e.g. `image/png`), as the `Content-Type`
    /// declared it. A host that inlines this as a `data:` URI is responsible for its own
    /// validation — the bytes are hostile input.
    media_type: String,
    /// The content-transfer-decoded part bytes.
    bytes: Vec<u8>,
}

impl InlinePart {
    /// Creates an inline part from its `cid` token (angle brackets already stripped), its
    /// media type, and its decoded bytes.
    #[must_use]
    pub fn new(
        content_id: impl Into<String>,
        media_type: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            content_id: content_id.into(),
            media_type: media_type.into(),
            bytes,
        }
    }

    /// The `Content-ID` token (no angle brackets) a `cid:` URL references.
    #[must_use]
    pub fn content_id(&self) -> &str {
        &self.content_id
    }

    /// The media type (`Content-Type` with parameters stripped, e.g. `image/png`).
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The content-transfer-decoded bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for InlinePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlinePart")
            .field("content_id", &self.content_id)
            .field("media_type", &self.media_type)
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

/// Stable index of an attachment part inside a parsed raw RFC 5322 message.
///
/// The id is derived from the immutable raw source cached for a message, so it is stable
/// for that source and intentionally scoped to one message. Hosts pass it back when the
/// user downloads one listed attachment; it is not a provider object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachmentPartId(u32);

impl AttachmentPartId {
    /// Creates an attachment-part id from the parser's attachment index.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The zero-based parser attachment index.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for AttachmentPartId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AttachmentPartId").field(&self.0).finish()
    }
}

/// Metadata for one downloadable attachment decoded from a raw message source.
///
/// This is a derived view of the cached raw MIME, not stored state. It deliberately
/// carries metadata only; the bytes are fetched for a selected id through
/// [`MessageAttachmentContent`], so a reading snapshot can list attachments without
/// copying their content.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttachment {
    id: AttachmentPartId,
    file_name: String,
    media_type: String,
    size: u64,
    inline: bool,
    content_id: Option<String>,
}

impl MessageAttachment {
    /// Creates attachment metadata.
    #[must_use]
    pub fn new(
        id: AttachmentPartId,
        file_name: impl Into<String>,
        media_type: impl Into<String>,
        size: u64,
        inline: bool,
        content_id: Option<String>,
    ) -> Self {
        Self {
            id,
            file_name: file_name.into(),
            media_type: media_type.into(),
            size,
            inline,
            content_id,
        }
    }

    /// The message-scoped attachment part id.
    #[must_use]
    pub const fn id(&self) -> AttachmentPartId {
        self.id
    }

    /// Suggested display/download file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The media type (`Content-Type` with parameters stripped).
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The decoded byte length.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Whether the part was marked inline rather than as a regular attachment.
    #[must_use]
    pub const fn is_inline(&self) -> bool {
        self.inline
    }

    /// The `Content-ID` token, when present.
    #[must_use]
    pub fn content_id(&self) -> Option<&str> {
        self.content_id.as_deref()
    }
}

impl fmt::Debug for MessageAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageAttachment")
            .field("id", &self.id)
            .field("file_name_len", &self.file_name.len())
            .field("media_type", &self.media_type)
            .field("size", &self.size)
            .field("inline", &self.inline)
            .field("has_content_id", &self.content_id.is_some())
            .finish()
    }
}

/// The selected attachment's metadata plus decoded bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttachmentContent {
    attachment: MessageAttachment,
    bytes: Vec<u8>,
}

impl MessageAttachmentContent {
    /// Creates selected attachment content.
    #[must_use]
    pub fn new(attachment: MessageAttachment, bytes: Vec<u8>) -> Self {
        Self { attachment, bytes }
    }

    /// Metadata for the selected attachment.
    #[must_use]
    pub fn attachment(&self) -> &MessageAttachment {
        &self.attachment
    }

    /// The decoded attachment bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes this value and returns its decoded bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for MessageAttachmentContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageAttachmentContent")
            .field("attachment", &self.attachment)
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_expose_each_part() {
        let body = MessageBody::new(Some("hello".to_owned()), Some("<p>hi</p>".to_owned()));
        assert_eq!(body.plain(), Some("hello"));
        assert_eq!(body.html(), Some("<p>hi</p>"));
        assert!(!body.is_empty());
    }

    #[test]
    fn empty_and_blank_parts_are_empty() {
        assert!(MessageBody::empty().is_empty());
        // A present-but-empty part still counts as nothing to show.
        assert!(MessageBody::new(Some(String::new()), None).is_empty());
        assert!(MessageBody::new(None, Some(String::new())).is_empty());
        // Plain text present, no HTML: not empty, and `html()` is `None`.
        let plain_only = MessageBody::new(Some("body".to_owned()), None);
        assert!(!plain_only.is_empty());
        assert_eq!(plain_only.html(), None);
    }

    #[test]
    fn roundtrips_through_json() {
        let body = MessageBody::new(Some("plain".to_owned()), Some("<i>x</i>".to_owned()));
        let json = serde_json::to_string(&body).unwrap();
        assert_eq!(serde_json::from_str::<MessageBody>(&json).unwrap(), body);
    }

    #[test]
    fn debug_is_redacted() {
        let body = MessageBody::new(
            Some("secret body".to_owned()),
            Some("<b>private</b>".to_owned()),
        );
        let shown = format!("{body:?}");
        assert!(shown.contains("plain_len: Some(11)"), "{shown}");
        assert!(shown.contains("html_len: Some(14)"), "{shown}");
        assert!(
            !shown.contains("secret") && !shown.contains("private"),
            "body content must not leak: {shown}"
        );
    }

    #[test]
    fn inline_part_accessors_expose_each_field() {
        let part = InlinePart::new("logo@x", "image/png", vec![1, 2, 3]);
        assert_eq!(part.content_id(), "logo@x");
        assert_eq!(part.media_type(), "image/png");
        assert_eq!(part.bytes(), &[1, 2, 3]);
    }

    #[test]
    fn inline_part_roundtrips_through_json() {
        let part = InlinePart::new("chart.1@host", "image/gif", vec![0xde, 0xad, 0xbe, 0xef]);
        let json = serde_json::to_string(&part).unwrap();
        assert_eq!(serde_json::from_str::<InlinePart>(&json).unwrap(), part);
    }

    #[test]
    fn inline_part_debug_redacts_bytes() {
        // The id and media type are not sensitive (they are routing metadata), but the
        // bytes are content — only their length may print.
        let part = InlinePart::new("logo@x", "image/png", b"\x89PNGsecretpixels".to_vec());
        let shown = format!("{part:?}");
        assert!(shown.contains("content_id: \"logo@x\""), "{shown}");
        assert!(shown.contains("media_type: \"image/png\""), "{shown}");
        assert!(shown.contains("bytes_len: 16"), "{shown}");
        assert!(
            !shown.contains("secretpixels") && !shown.contains("PNG"),
            "inline bytes must not leak: {shown}"
        );
    }

    #[test]
    fn attachment_metadata_exposes_fields_without_bytes() {
        let id = AttachmentPartId::new(7);
        let attachment =
            MessageAttachment::new(id, "report.pdf", "application/pdf", 1234, false, None);
        assert_eq!(attachment.id().as_u32(), 7);
        assert_eq!(attachment.file_name(), "report.pdf");
        assert_eq!(attachment.media_type(), "application/pdf");
        assert_eq!(attachment.size(), 1234);
        assert!(!attachment.is_inline());
        assert_eq!(attachment.content_id(), None);
    }

    #[test]
    fn attachment_content_debug_redacts_bytes_and_filename() {
        let attachment = MessageAttachment::new(
            AttachmentPartId::new(1),
            "confidential plan.pdf",
            "application/pdf",
            10,
            false,
            None,
        );
        let content = MessageAttachmentContent::new(attachment, b"secret pdf".to_vec());
        assert_eq!(content.bytes(), b"secret pdf");
        let shown = format!("{content:?}");
        assert!(shown.contains("bytes_len: 10"), "{shown}");
        assert!(
            !shown.contains("secret") && !shown.contains("confidential"),
            "attachment content/filename must not leak: {shown}"
        );
    }
}
