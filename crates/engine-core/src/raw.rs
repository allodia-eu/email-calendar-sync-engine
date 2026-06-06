//! Preserved provider-native payloads.
//!
//! The engine keeps each provider's original bytes beside the normalized
//! projection so data can be re-parsed and provider writes can round-trip from
//! raw plus targeted patches, never by re-serializing the lossy projection
//! (`calendar-semantics.md`). MIME, iCalendar, and JSCalendar each get an
//! explicit raw type.
//!
//! All three implement a **redacted** `Debug` that prints only the payload
//! length, never its content: mail and calendar bodies are sensitive data and
//! logs are redacted by default (`north-star.md` security section). Use the
//! accessors to reach the bytes deliberately.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Raw RFC 5322 message bytes (the MIME source referenced by a JMAP `blobId`).
///
/// This is the lossless source the normalized [`crate`] mail projection is
/// derived from. It is held as a transient byte container — large raw messages
/// are tier-3 content fetched on demand and persisted by the store as an
/// out-of-band blob, so this type deliberately does not implement `serde`.
#[derive(Clone, PartialEq, Eq)]
pub struct RawMime(Box<[u8]>);

impl RawMime {
    /// Wraps raw message bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into().into_boxed_slice())
    }

    /// Returns the raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the length of the payload in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for RawMime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawMime")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// Defines a text-backed raw payload newtype with a redacted `Debug` and a
/// transparent string `serde` representation.
macro_rules! raw_text {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Box<str>);

        impl $name {
            #[doc = "Wraps the raw payload text verbatim."]
            #[must_use]
            pub fn new(text: impl Into<String>) -> Self {
                Self(text.into().into_boxed_str())
            }

            #[doc = "Returns the payload as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = "Returns the length of the payload in bytes."]
            #[must_use]
            pub fn len(&self) -> usize {
                self.0.len()
            }

            #[doc = "Returns `true` if the payload is empty."]
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("len", &self.0.len())
                    .finish_non_exhaustive()
            }
        }
    };
}

raw_text! {
    /// Raw iCalendar text (`text/calendar`, RFC 5545), preserved verbatim.
    ///
    /// Kept beside the JSCalendar projection because the projection is lossy
    /// (`VALARM` nuance, `ATTACH`, some `X-` properties, `THISANDFUTURE`
    /// semantics) and is explicitly **not** round-trip-authoritative. An
    /// embedded `VTIMEZONE` that disagrees with IANA is preserved here so the
    /// chosen expansion source can be recorded (`calendar-semantics.md`).
    RawIcal
}

raw_text! {
    /// Raw JSCalendar JSON text (RFC 8984), preserved verbatim.
    ///
    /// Held as the original text rather than a parsed value so object key order
    /// and unknown/vendor-extension properties survive losslessly for
    /// re-derivation and provider writes.
    RawJsCalendar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_mime_exposes_bytes_and_length() {
        let raw = RawMime::new(b"From: a@b\r\n\r\nhi".to_vec());
        assert_eq!(raw.len(), 15);
        assert!(!raw.is_empty());
        assert_eq!(&raw.as_bytes()[..6], b"From: ");
        assert!(RawMime::new(Vec::new()).is_empty());
    }

    #[test]
    fn raw_mime_debug_is_redacted() {
        let raw = RawMime::new(b"secret body".to_vec());
        let shown = format!("{raw:?}");
        assert!(shown.contains("len: 11"), "{shown}");
        assert!(
            !shown.contains("secret"),
            "raw content must not leak: {shown}"
        );
    }

    #[test]
    fn raw_text_debug_is_redacted() {
        let ical = RawIcal::new("BEGIN:VCALENDAR\r\nSECRET-LOCATION:hospital");
        let shown = format!("{ical:?}");
        assert!(shown.contains("RawIcal"));
        assert!(
            !shown.contains("hospital"),
            "raw content must not leak: {shown}"
        );
    }

    #[test]
    fn raw_text_preserves_payload_verbatim_through_json() {
        let jscal = RawJsCalendar::new(r#"{"@type":"Event","uid":"x","keep":"order"}"#);
        let json = serde_json::to_string(&jscal).unwrap();
        let back: RawJsCalendar = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), jscal.as_str());
        assert!(!back.is_empty());
    }
}
