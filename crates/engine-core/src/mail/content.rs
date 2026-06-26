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
}
