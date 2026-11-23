//! What a message carries beside its bytes and its placement.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::Keyword;
use crate::time::UtcDateTime;

/// What a message carries beside its bytes and the mailboxes it is filed in: the state a write
/// asks a target to store, and that a target may or may not keep.
///
/// It is the one vocabulary for that, on every write that stores a message (a draft, the
/// sender's copy of a sent message, an import) and on every receipt that says what was kept:
/// the request carries the metadata it asks for, the receipt the metadata the target stored, so
/// a target that keeps less says so per message, field by field. A field added here (Microsoft
/// Graph's `categories` is the next) reaches every write path at once.
///
/// A message read from a source hands its own over with [`Message::metadata`], the one mapping
/// from what is read to what is written.
///
/// Built with [`Default`] and the `with_*` methods, so a field added later does not break a
/// caller.
///
/// # Serialized form
///
/// A map of its fields, each defaulted when absent. A bare list of keywords also reads, as
/// metadata carrying only those keywords: the form a draft's keywords were persisted in before
/// this type existed, so an outbox written then still loads.
///
/// [`Message::metadata`]: super::Message::metadata
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "MetadataWire")]
#[non_exhaustive]
pub struct MessageMetadata {
    /// The keywords (`$seen`, `$flagged`, and custom ones).
    pub keywords: BTreeSet<Keyword>,
    /// When the message arrived (IMAP `INTERNALDATE`, JMAP `receivedAt`), or `None` to let the
    /// target stamp the time it stores the message.
    ///
    /// Only a write that stores an existing message as it was (an import) can set it; a draft
    /// or a sent copy is stamped by the target, so it never keeps one asked for.
    pub received_at: Option<UtcDateTime>,
}

impl MessageMetadata {
    /// Carries `keywords` (builder-style).
    #[must_use]
    pub fn with_keywords(mut self, keywords: BTreeSet<Keyword>) -> Self {
        self.keywords = keywords;
        self
    }

    /// Carries `keyword` besides those it already carries (builder-style).
    #[must_use]
    pub fn with_keyword(mut self, keyword: Keyword) -> Self {
        self.keywords.insert(keyword);
        self
    }

    /// Carries `at` as the time the message arrived (builder-style).
    #[must_use]
    pub fn with_received_at(mut self, at: UtcDateTime) -> Self {
        self.received_at = Some(at);
        self
    }

    /// Whether it carries nothing: no keyword and no arrival time.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The forms [`MessageMetadata`] deserializes from: its own, and a bare keyword list.
#[derive(Deserialize)]
#[serde(untagged)]
enum MetadataWire {
    Keywords(BTreeSet<Keyword>),
    Fields {
        #[serde(default)]
        keywords: BTreeSet<Keyword>,
        #[serde(default)]
        received_at: Option<UtcDateTime>,
    },
}

impl From<MetadataWire> for MessageMetadata {
    fn from(wire: MetadataWire) -> Self {
        match wire {
            MetadataWire::Keywords(keywords) => Self {
                keywords,
                received_at: None,
            },
            MetadataWire::Fields {
                keywords,
                received_at,
            } => Self {
                keywords,
                received_at,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword(name: &str) -> Keyword {
        Keyword::new(name).unwrap()
    }

    #[test]
    fn metadata_round_trips_through_its_serialized_form() {
        let metadata = MessageMetadata::default()
            .with_keyword(keyword("$flagged"))
            .with_received_at("2019-02-03T04:05:06Z".parse().unwrap());

        let json = serde_json::to_string(&metadata).unwrap();
        let back: MessageMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(back, metadata);
    }

    #[test]
    fn a_bare_keyword_list_reads_as_metadata_carrying_those_keywords() {
        let read: MessageMetadata = serde_json::from_str(r#"["$flagged","project-x"]"#).unwrap();

        assert_eq!(
            read,
            MessageMetadata::default()
                .with_keywords(BTreeSet::from([keyword("$flagged"), keyword("project-x")]))
        );
    }

    #[test]
    fn a_missing_field_reads_as_its_default() {
        let read: MessageMetadata = serde_json::from_str(r#"{"keywords":["$seen"]}"#).unwrap();
        assert_eq!(read.received_at, None);
        let read: MessageMetadata = serde_json::from_str("{}").unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn metadata_is_empty_only_when_it_carries_nothing() {
        assert!(MessageMetadata::default().is_empty());
        assert!(
            !MessageMetadata::default()
                .with_keyword(keyword("$seen"))
                .is_empty()
        );
        assert!(
            !MessageMetadata::default()
                .with_received_at("2019-02-03T04:05:06Z".parse().unwrap())
                .is_empty()
        );
    }
}
