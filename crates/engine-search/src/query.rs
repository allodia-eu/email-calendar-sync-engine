//! The store-agnostic search query AST.
//!
//! A [`Query`] is scoped to one domain. Mail and calendar have disjoint filter
//! vocabularies and execute against different indexes (`north-star.md` Search
//! Contract), so the AST keeps them as separate variants rather than a single bag
//! of optional fields. Both share the [`TextQuery`] shape for free text.
//!
//! The structured filters here map onto normalized index tables and junctions;
//! the text terms map onto the store's full-text engine. The DSL→schema mapping
//! that fixes which filter goes where is recorded in the search handoff and in
//! the `store-sqlite` V2 schema. This module models *what was asked*; it does not
//! decide match semantics (exact vs substring) — that is the executor's job.
//!
//! Fields are public because these are plain parsed-data carriers with no
//! cross-field invariant to preserve (like [`engine_core::mail`]'s `Envelope`).
//! An all-empty query is valid and matches everything in scope.

use engine_core::calendar::ParticipationStatus;
use engine_core::time::CalendarDate;
use serde::{Deserialize, Serialize};

/// A parsed search query, scoped to one domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    /// A mail search over messages.
    Mail(MailQuery),
    /// A calendar search over events and their occurrences.
    Calendar(CalendarQuery),
}

/// A full-text component matched through the store's FTS engine.
///
/// `unscoped` terms match across all of a domain's indexed text fields; each
/// `scoped` term restricts to one field. The DSL routes `subject:` (mail) and
/// `location:` (calendar) here rather than to a structured filter, because both
/// are full-text matches in the index, not equality lookups. Each term is a word
/// or a quoted phrase; the executor combines a domain's terms (FTS ANDs them by
/// default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextQuery {
    /// Terms matched across every indexed text field of the domain.
    pub unscoped: Vec<String>,
    /// Terms restricted to a single field.
    pub scoped: Vec<ScopedTerm>,
}

impl TextQuery {
    /// Returns `true` if there is no text to match.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.unscoped.is_empty() && self.scoped.is_empty()
    }
}

/// A full-text term restricted to one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedTerm {
    /// The field the term is restricted to.
    pub field: TextField,
    /// The word or phrase to match.
    pub text: String,
}

/// A text field that a [`ScopedTerm`] can restrict to.
///
/// Only the fields the DSL exposes as scoping operators appear here: `subject:`
/// for mail and `location:` for calendar. Bodies and descriptions are matched
/// through unscoped free text, not a scoping operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextField {
    /// A message subject (mail).
    Subject,
    /// An event location (calendar).
    Location,
}

/// A mail search: free text plus the mail filter vocabulary.
///
/// `from`/`to`/`cc` are address-junction lookups; `mailbox`/`label`/`keyword` are
/// membership-junction lookups; `before`/`after` bound the message date;
/// `has_attachment` is a scalar. Repeating an operator accumulates into the
/// corresponding list (e.g. two `from:` values), which the executor treats as
/// alternatives for that field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailQuery {
    /// Free text and `subject:`-scoped text.
    pub text: TextQuery,
    /// `from:` address terms.
    pub from: Vec<String>,
    /// `to:` address terms.
    pub to: Vec<String>,
    /// `cc:` address terms.
    pub cc: Vec<String>,
    /// `mailbox:` membership values.
    pub mailbox: Vec<String>,
    /// `label:` membership values.
    pub label: Vec<String>,
    /// `keyword:` membership values.
    pub keyword: Vec<String>,
    /// `after:` lower date bound (inclusive), if given.
    pub after: Option<CalendarDate>,
    /// `before:` upper date bound (exclusive), if given.
    pub before: Option<CalendarDate>,
    /// `has_attachment:` scalar filter, if given.
    pub has_attachment: Option<bool>,
}

/// A calendar search: free text plus the calendar filter vocabulary.
///
/// `calendar` is a membership-junction lookup; `attendee`/`organizer` are
/// participant-junction lookups; `rsvp` filters on the stored participation
/// status; `before`/`after` bound the occurrence time range; `has_conference` is
/// a scalar. `location:` text is carried in [`Self::text`] as a [`ScopedTerm`],
/// not as a structured filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarQuery {
    /// Free text and `location:`-scoped text.
    pub text: TextQuery,
    /// `calendar:` membership values.
    pub calendar: Vec<String>,
    /// `attendee:` participant address terms.
    pub attendee: Vec<String>,
    /// `organizer:` participant address terms.
    pub organizer: Vec<String>,
    /// `rsvp:` participation-status filters.
    pub rsvp: Vec<ParticipationStatus>,
    /// `after:` lower bound of the occurrence range (inclusive), if given.
    pub after: Option<CalendarDate>,
    /// `before:` upper bound of the occurrence range (exclusive), if given.
    pub before: Option<CalendarDate>,
    /// `has_conference:` scalar filter, if given.
    pub has_conference: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> CalendarDate {
        s.parse().unwrap()
    }

    #[test]
    fn empty_queries_are_the_defaults() {
        assert!(MailQuery::default().text.is_empty());
        assert!(CalendarQuery::default().text.is_empty());
        assert!(MailQuery::default().from.is_empty());
    }

    #[test]
    fn text_query_emptiness_tracks_both_kinds() {
        let mut t = TextQuery::default();
        assert!(t.is_empty());
        t.unscoped.push("hello".into());
        assert!(!t.is_empty());

        let mut s = TextQuery::default();
        s.scoped.push(ScopedTerm {
            field: TextField::Subject,
            text: "invoice".into(),
        });
        assert!(!s.is_empty());
    }

    #[test]
    fn mail_query_roundtrips_through_json() {
        let query = MailQuery {
            text: TextQuery {
                unscoped: vec!["quarterly report".into()],
                scoped: vec![ScopedTerm {
                    field: TextField::Subject,
                    text: "invoice".into(),
                }],
            },
            from: vec!["alice@example.com".into()],
            to: vec!["bob@example.com".into()],
            cc: vec![],
            mailbox: vec!["inbox".into()],
            label: vec!["work".into()],
            keyword: vec!["$flagged".into()],
            after: Some(date("2026-01-01")),
            before: Some(date("2026-04-01")),
            has_attachment: Some(true),
        };
        let json = serde_json::to_string(&query).unwrap();
        assert_eq!(serde_json::from_str::<MailQuery>(&json).unwrap(), query);
    }

    #[test]
    fn calendar_query_roundtrips_through_json() {
        let query = CalendarQuery {
            text: TextQuery {
                unscoped: vec!["standup".into()],
                scoped: vec![ScopedTerm {
                    field: TextField::Location,
                    text: "room 4".into(),
                }],
            },
            calendar: vec!["work".into()],
            attendee: vec!["carol@example.com".into()],
            organizer: vec!["dave@example.com".into()],
            rsvp: vec![
                ParticipationStatus::Accepted,
                ParticipationStatus::Tentative,
            ],
            after: Some(date("2026-06-01")),
            before: Some(date("2026-07-01")),
            has_conference: Some(false),
        };
        let json = serde_json::to_string(&query).unwrap();
        assert_eq!(serde_json::from_str::<CalendarQuery>(&json).unwrap(), query);
    }

    #[test]
    fn query_enum_wraps_either_domain() {
        let mail = Query::Mail(MailQuery::default());
        let cal = Query::Calendar(CalendarQuery::default());
        assert_ne!(
            serde_json::to_string(&mail).unwrap(),
            serde_json::to_string(&cal).unwrap()
        );
        // The two domains are distinct variants, so a mail query can never be
        // mistaken for a calendar one at the type level.
        assert!(matches!(mail, Query::Mail(_)));
        assert!(matches!(cal, Query::Calendar(_)));
    }
}
