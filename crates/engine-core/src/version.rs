//! Per-object revision tokens for optimistic concurrency.
//!
//! Provider object keys are stable across moves, but each provider tracks
//! *revisions* with its own token, and their change-semantics differ — so they
//! are kept as distinct types, never unified into one "version string"
//! (`modeling.md`):
//!
//! - [`ETag`] — CalDAV `getetag` / Microsoft Graph `ETag`; changes on any byte
//!   change.
//! - [`ScheduleTag`] — CalDAV scheduling `schedule-tag` (RFC 6638); changes only
//!   on *consequential* changes, so an attendee's reply to your copy does not
//!   bump it. A CalDAV scheduling resource carries **both** an `ETag` and a
//!   `ScheduleTag` at once.
//! - [`ChangeKey`] — Microsoft Graph `changeKey`.
//! - [`ModSeq`] — IMAP CONDSTORE per-message mod-sequence (RFC 7162), present
//!   only when the optional capability is enabled.
//!
//! JMAP objects carry **no** per-object token; their concurrency comes from the
//! account-and-type `state` cursor instead, so a JMAP object has empty
//! [`RevisionTokens`].

use serde::{Deserialize, Serialize};

/// Defines an opaque string-backed revision-token newtype.
macro_rules! string_token {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Box<str>);

        impl $name {
            #[doc = "Wraps the provider's token value verbatim."]
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into().into_boxed_str())
            }

            #[doc = "Returns the token as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

string_token! {
    /// An HTTP entity tag (CalDAV `getetag`, Graph `ETag`). Compared verbatim;
    /// the engine never parses weak/strong syntax.
    ETag
}

string_token! {
    /// A CalDAV scheduling `schedule-tag` (RFC 6638 §3.2.10). Distinguishes
    /// consequential from inconsequential changes; coexists with an [`ETag`].
    ScheduleTag
}

string_token! {
    /// A Microsoft Graph `changeKey` revision token.
    ChangeKey
}

/// An IMAP CONDSTORE per-message mod-sequence (RFC 7162). A monotonic counter
/// bumped on any metadata or flag change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModSeq(u64);

impl ModSeq {
    /// Wraps a raw mod-sequence value.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw mod-sequence value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The set of revision tokens a provider supplied for one object.
///
/// Any subset may be present: CalDAV scheduling resources set both `etag` and
/// `schedule_tag`; plain CalDAV sets only `etag`; Graph sets `change_key`; IMAP
/// sets `mod_seq` under CONDSTORE; JMAP sets none. The struct simply records
/// which the provider gave, without asserting a particular combination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RevisionTokens {
    /// The entity tag, if the provider supplied one.
    pub etag: Option<ETag>,
    /// The scheduling tag, if this is a CalDAV scheduling resource.
    pub schedule_tag: Option<ScheduleTag>,
    /// The Microsoft Graph change key, if applicable.
    pub change_key: Option<ChangeKey>,
    /// The IMAP CONDSTORE mod-sequence, if the capability is enabled.
    pub mod_seq: Option<ModSeq>,
}

impl RevisionTokens {
    /// Returns an empty set of tokens, as carried by JMAP objects.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Returns a set carrying only the given entity tag.
    #[must_use]
    pub fn from_etag(etag: ETag) -> Self {
        Self {
            etag: Some(etag),
            ..Self::default()
        }
    }

    /// Returns `true` if no revision token is present (the JMAP case).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.etag.is_none()
            && self.schedule_tag.is_none()
            && self.change_key.is_none()
            && self.mod_seq.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jmap_object_has_no_revision_token() {
        assert!(RevisionTokens::none().is_empty());
    }

    #[test]
    fn caldav_scheduling_resource_carries_etag_and_schedule_tag() {
        let tokens = RevisionTokens {
            etag: Some(ETag::new("\"abc\"")),
            schedule_tag: Some(ScheduleTag::new("\"sched-1\"")),
            ..RevisionTokens::default()
        };
        assert!(!tokens.is_empty());
        assert_eq!(tokens.etag.as_ref().unwrap().as_str(), "\"abc\"");
        assert_eq!(tokens.schedule_tag.unwrap().as_str(), "\"sched-1\"");
    }

    #[test]
    fn mod_seq_roundtrips() {
        let m = ModSeq::new(42);
        assert_eq!(m.get(), 42);
        assert!(ModSeq::new(1) < ModSeq::new(2));
    }

    #[test]
    fn revision_tokens_roundtrip_through_json() {
        let tokens = RevisionTokens::from_etag(ETag::new("v1"));
        let json = serde_json::to_string(&tokens).unwrap();
        let back: RevisionTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(tokens, back);
        // An empty object deserializes to "no tokens".
        let empty: RevisionTokens = serde_json::from_str("{}").unwrap();
        assert!(empty.is_empty());
    }
}
