//! Attachments.
//!
//! Attachments span four kinds (`modeling.md`), and the distinction is
//! load-bearing because quota and host-open policy apply to byte content only:
//!
//! - **file** — bytes (a MIME part, a Graph `fileAttachment`, a Google Drive
//!   file fetched as bytes);
//! - **inline** — bytes referenced from the body by `Content-ID` (a `cid:` URL);
//! - **item** — an embedded message, event, or contact (`message/rfc822`, a
//!   Graph `itemAttachment`);
//! - **reference** — an external/cloud link with no bytes (a Graph
//!   `referenceAttachment`, a Google Calendar `attachment` `fileUrl`, an
//!   iCalendar `ATTACH` URI).
//!
//! This one type serves both mail and calendar attachments.

use serde::{Deserialize, Serialize};

use crate::ids::BlobId;

/// Common attachment metadata shared by every kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    /// The display/file name, if any.
    pub name: Option<String>,
    /// The MIME media type (e.g. `application/pdf`), if known.
    pub media_type: Option<String>,
    /// The size in bytes, if known. Never set for a reference attachment.
    pub size: Option<u64>,
}

/// The embedded-item kind for an [`Attachment::Item`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    /// An embedded message (`message/rfc822`).
    Message,
    /// An embedded calendar event.
    Event,
    /// An embedded contact.
    Contact,
    /// A provider-specific embedded item kind preserved verbatim.
    Other(String),
}

/// A normalized attachment.
///
/// Each variant carries shared [`AttachmentMeta`] plus kind-specific data. Use
/// [`Attachment::blob`] / [`Attachment::has_bytes`] to decide whether quota and
/// host-open policy apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attachment {
    /// A file attachment with byte content.
    File {
        /// Shared metadata.
        meta: AttachmentMeta,
        /// The blob holding the bytes, once fetched.
        blob: Option<BlobId>,
    },
    /// An inline attachment referenced from the body by its content id.
    Inline {
        /// Shared metadata.
        meta: AttachmentMeta,
        /// The `Content-ID` (without angle brackets) the body refers to.
        cid: String,
        /// The blob holding the bytes, once fetched.
        blob: Option<BlobId>,
    },
    /// An embedded message, event, or contact.
    Item {
        /// Shared metadata.
        meta: AttachmentMeta,
        /// The kind of embedded item.
        item: ItemKind,
        /// The blob holding the serialized item, once fetched.
        blob: Option<BlobId>,
    },
    /// An external/cloud link with no byte content.
    Reference {
        /// Shared metadata (`size` is always `None`).
        meta: AttachmentMeta,
        /// The external URI.
        uri: String,
    },
}

impl Attachment {
    /// Returns the shared metadata for any kind.
    #[must_use]
    pub fn meta(&self) -> &AttachmentMeta {
        match self {
            Self::File { meta, .. }
            | Self::Inline { meta, .. }
            | Self::Item { meta, .. }
            | Self::Reference { meta, .. } => meta,
        }
    }

    /// Returns the blob backing this attachment's bytes, if any. A reference
    /// attachment never has one; the other kinds have one once fetched.
    #[must_use]
    pub fn blob(&self) -> Option<&BlobId> {
        match self {
            Self::File { blob, .. } | Self::Inline { blob, .. } | Self::Item { blob, .. } => {
                blob.as_ref()
            }
            Self::Reference { .. } => None,
        }
    }

    /// Returns `true` if this attachment can carry byte content (so quota and
    /// host-open policy apply). Only a reference attachment cannot.
    #[must_use]
    pub fn has_bytes(&self) -> bool {
        !matches!(self, Self::Reference { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> AttachmentMeta {
        AttachmentMeta {
            name: Some("report.pdf".into()),
            media_type: Some("application/pdf".into()),
            size: Some(1024),
        }
    }

    #[test]
    fn reference_has_no_bytes_and_no_blob() {
        let att = Attachment::Reference {
            meta: AttachmentMeta {
                name: Some("Design".into()),
                media_type: None,
                size: None,
            },
            uri: "https://drive.example/abc".into(),
        };
        assert!(!att.has_bytes());
        assert!(att.blob().is_none());
        assert_eq!(att.meta().name.as_deref(), Some("Design"));
    }

    #[test]
    fn byte_kinds_report_bytes_and_carry_blobs() {
        let blob = BlobId::try_from("blob-1").unwrap();
        let file = Attachment::File {
            meta: meta(),
            blob: Some(blob.clone()),
        };
        assert!(file.has_bytes());
        assert_eq!(file.blob(), Some(&blob));

        let inline = Attachment::Inline {
            meta: meta(),
            cid: "image001@example".into(),
            blob: None,
        };
        assert!(inline.has_bytes());
        assert!(inline.blob().is_none()); // not yet fetched

        let item = Attachment::Item {
            meta: AttachmentMeta::default(),
            item: ItemKind::Message,
            blob: None,
        };
        assert!(item.has_bytes());
    }

    #[test]
    fn roundtrips_through_json() {
        let att = Attachment::Item {
            meta: meta(),
            item: ItemKind::Event,
            blob: None,
        };
        let json = serde_json::to_string(&att).unwrap();
        assert_eq!(serde_json::from_str::<Attachment>(&json).unwrap(), att);
    }
}
