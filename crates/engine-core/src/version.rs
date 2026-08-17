//! Per-object revision tokens for optimistic concurrency.
//!
//! Provider object keys are stable across moves, but each provider tracks
//! *revisions* with its own token, and their change-semantics differ — so they
//! are kept as distinct types, never unified into one "version string"
//! (`modeling.md`):
//!
//! - [`ETag`] — CalDAV `getetag` / Microsoft Graph `ETag`; changes on any byte change.
//! - [`ScheduleTag`] — CalDAV scheduling `schedule-tag` (RFC 6638); changes only on *consequential*
//!   changes, so an attendee's reply to your copy does not bump it. A CalDAV scheduling resource
//!   carries **both** an `ETag` and a `ScheduleTag` at once.
//! - [`ChangeKey`] — Microsoft Graph `changeKey`.
//! - [`ModSeq`] — IMAP CONDSTORE per-message mod-sequence (RFC 7162), present only when the
//!   optional capability is enabled.
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

    /// These tokens, taking each one they are **silent** about from `prior` — field by field,
    /// exactly [`Option::or`].
    ///
    /// For applying a **partial** report over a stored set. A partial names the tokens that
    /// moved and says nothing about the rest, so a `None` in it means *not reported*, never
    /// *gone*. Writing one verbatim would blank a token a later conditional write has to quote,
    /// and a write that quotes nothing is an unguarded last-writer-wins — the failure is silent
    /// data loss, not an error anyone sees.
    ///
    /// This is provider-neutral because the silence is: Gmail's history record and JMAP's
    /// `Email/changes` carry no token at all, IMAP's `FLAGS` row carries no `MODSEQ` unless it
    /// was asked for, and Graph's narrow `$select` may answer without the `@odata.etag` a full
    /// message resource would have carried. A **whole object** is authoritative about every
    /// token and replaces them instead — it does not come through here.
    #[must_use]
    pub fn or(self, prior: &Self) -> Self {
        Self {
            etag: self.etag.or_else(|| prior.etag.clone()),
            schedule_tag: self.schedule_tag.or_else(|| prior.schedule_tag.clone()),
            change_key: self.change_key.or_else(|| prior.change_key.clone()),
            mod_seq: self.mod_seq.or(prior.mod_seq),
        }
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
    fn a_silent_token_is_taken_from_the_prior_set_and_a_named_one_replaces_it() {
        let stored = RevisionTokens {
            etag: Some(ETag::new("W/\"stored\"")),
            change_key: Some(ChangeKey::new("stored-key")),
            mod_seq: Some(ModSeq::new(7)),
            schedule_tag: Some(ScheduleTag::new("\"sched\"")),
        };
        // What a Graph state-only `$select` answers when it omits the etag annotation: the
        // changeKey moved, and the rest of the set was never mentioned.
        let reported = RevisionTokens {
            change_key: Some(ChangeKey::new("fresh-key")),
            ..RevisionTokens::none()
        };
        let merged = reported.or(&stored);
        assert_eq!(
            merged.change_key.as_ref().map(ChangeKey::as_str),
            Some("fresh-key"),
            "the token the report named moved"
        );
        assert_eq!(
            merged.etag.as_ref().map(ETag::as_str),
            Some("W/\"stored\""),
            "and the one it was silent about survives — blanking it disarms the next If-Match"
        );
        assert_eq!(merged.mod_seq, Some(ModSeq::new(7)));
        assert_eq!(
            merged.schedule_tag.as_ref().map(ScheduleTag::as_str),
            Some("\"sched\"")
        );
    }

    #[test]
    fn a_wholly_silent_report_changes_nothing() {
        // Gmail's history record and JMAP's `Email/changes` carry no token at all, so every
        // state change those two produce lands here.
        let stored = RevisionTokens::from_etag(ETag::new("v1"));
        assert_eq!(RevisionTokens::none().or(&stored), stored);
        // And over an empty prior set it stays empty rather than inventing one.
        assert!(
            RevisionTokens::none()
                .or(&RevisionTokens::none())
                .is_empty()
        );
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
