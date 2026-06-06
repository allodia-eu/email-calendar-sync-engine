//! Normalized MIME body structure.

use serde::{Deserialize, Serialize};

use super::EmailHeader;
use crate::ids::{BlobId, PartId};

/// A node in the normalized MIME tree (JMAP `EmailBodyPart`, RFC 8621 §4.1.4).
///
/// A leaf part (`text/plain`, an attachment, …) has a `part_id` and `blob_id`
/// and no `sub_parts`. A `multipart/*` container has neither id — its bytes are
/// its children — and a non-empty `sub_parts`. The body's text/HTML/attachment
/// decomposition is derived from this tree by the index layer; the algorithm is
/// provider-defined and not modeled here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailBodyPart {
    /// Identifies this part within its message; `None` for a `multipart/*`
    /// container.
    pub part_id: Option<PartId>,
    /// The blob of this part's decoded bytes; `None` for a `multipart/*`
    /// container.
    pub blob_id: Option<BlobId>,
    /// The size in octets after content-transfer-decoding (0 for a container).
    pub size: u64,
    /// The filename from `Content-Disposition` or `Content-Type`, if any.
    pub name: Option<String>,
    /// The media type with parameters stripped (e.g. `text/plain`). Defaults
    /// such as implicit `text/plain` are resolved by the adapter.
    pub media_type: String,
    /// The `charset` parameter, if any.
    pub charset: Option<String>,
    /// The `Content-Disposition` value with parameters stripped
    /// (e.g. `attachment`, `inline`), if any.
    pub disposition: Option<String>,
    /// The `Content-Id` with angle brackets removed (the target of a `cid:`
    /// reference), if any.
    pub cid: Option<String>,
    /// The `Content-Language` tags, if any.
    pub language: Vec<String>,
    /// The `Content-Location` URI, if any.
    pub location: Option<String>,
    /// The child parts for a `multipart/*` container; empty for a leaf.
    pub sub_parts: Vec<EmailBodyPart>,
    /// All headers of this part, in raw form and source order.
    pub headers: Vec<EmailHeader>,
}

impl EmailBodyPart {
    /// Creates a leaf part with the given id, blob, media type, and size.
    #[must_use]
    pub fn leaf(
        part_id: PartId,
        blob_id: BlobId,
        media_type: impl Into<String>,
        size: u64,
    ) -> Self {
        Self {
            part_id: Some(part_id),
            blob_id: Some(blob_id),
            size,
            name: None,
            media_type: media_type.into(),
            charset: None,
            disposition: None,
            cid: None,
            language: Vec::new(),
            location: None,
            sub_parts: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Creates a `multipart/*` container wrapping the given child parts.
    #[must_use]
    pub fn multipart(media_type: impl Into<String>, sub_parts: Vec<EmailBodyPart>) -> Self {
        Self {
            part_id: None,
            blob_id: None,
            size: 0,
            name: None,
            media_type: media_type.into(),
            charset: None,
            disposition: None,
            cid: None,
            language: Vec::new(),
            location: None,
            sub_parts,
            headers: Vec::new(),
        }
    }

    /// Returns `true` if this is a `multipart/*` container (no own bytes).
    #[must_use]
    pub fn is_multipart(&self) -> bool {
        self.part_id.is_none() && self.blob_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part_id(id: &str) -> PartId {
        PartId::try_from(id).unwrap()
    }

    fn blob_id(id: &str) -> BlobId {
        BlobId::try_from(id).unwrap()
    }

    #[test]
    fn leaf_and_container_distinguished() {
        let leaf = EmailBodyPart::leaf(part_id("1"), blob_id("b1"), "text/plain", 42);
        assert!(!leaf.is_multipart());
        assert_eq!(leaf.size, 42);

        let container = EmailBodyPart::multipart("multipart/alternative", vec![leaf.clone()]);
        assert!(container.is_multipart());
        assert_eq!(container.sub_parts.len(), 1);
        assert!(container.blob_id.is_none());
    }

    #[test]
    fn nested_structure_roundtrips() {
        let text = EmailBodyPart::leaf(part_id("1"), blob_id("b1"), "text/plain", 10);
        let html = EmailBodyPart::leaf(part_id("2"), blob_id("b2"), "text/html", 20);
        let alt = EmailBodyPart::multipart("multipart/alternative", vec![text, html]);
        let json = serde_json::to_string(&alt).unwrap();
        assert_eq!(serde_json::from_str::<EmailBodyPart>(&json).unwrap(), alt);
    }
}
