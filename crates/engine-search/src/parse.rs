//! The textual search DSL and its parser.
//!
//! The DSL is a space-separated list of terms. A term is either a `keyword:value`
//! operator or free text. The vocabularies are per domain (`north-star.md` Search
//! Contract):
//!
//! - **mail:** `from to cc subject has_attachment before after mailbox label
//!   keyword`
//! - **calendar:** `calendar attendee organizer rsvp location has_conference
//!   before after`
//!
//! Design choices (deliberately simple; revisit only with a real need):
//!
//! - **Only a known keyword before a colon is an operator.** Any other token —
//!   `http://example.com`, `3:1`, a misspelled `fromm:x` — is free text. There is
//!   no "unknown operator" error, so a query box never breaks on a stray colon.
//! - **Quoting** binds spaces: `subject:"q report"` is one scoped term;
//!   `"q report"` is one free-text phrase. An unbalanced quote is an error.
//! - **`before:`/`after:`** take a `YYYY-MM-DD` calendar date; the executor
//!   resolves it to a time boundary.
//! - **`has_attachment:`/`has_conference:`** take an explicit boolean
//!   (`true/false`, `yes/no`, `1/0`). A bare `has_attachment` with no value is
//!   free text — the host builds the explicit form.
//! - **`rsvp:`** accepts any participation-status string; unknown values are
//!   preserved (`ParticipationStatus::Other`), matching engine-core's open-enum
//!   stance. The executor simply will not match an unknown status.
//! - Operator keywords are matched case-insensitively; values are kept verbatim
//!   (the executor decides case-folding for matching).

use core::str::FromStr;

use engine_core::calendar::ParticipationStatus;
use engine_core::time::CalendarDate;

use crate::query::{CalendarQuery, MailQuery, ScopedTerm, TextField};

/// An error parsing a search query string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// A double quote was opened but never closed.
    #[error("unbalanced double quote in query")]
    UnbalancedQuote,
    /// An operator was given with no value (e.g. `from:`).
    #[error("the `{operator}` filter requires a value")]
    EmptyValue {
        /// The operator keyword that was missing a value.
        operator: String,
    },
    /// A `before:`/`after:` value was not a `YYYY-MM-DD` date.
    #[error("invalid date for `{operator}`: {value:?} (expected YYYY-MM-DD)")]
    InvalidDate {
        /// The operator keyword.
        operator: String,
        /// The offending value.
        value: String,
    },
    /// A `has_attachment:`/`has_conference:` value was not a boolean.
    #[error("invalid boolean for `{operator}`: {value:?} (expected true/false)")]
    InvalidBool {
        /// The operator keyword.
        operator: String,
        /// The offending value.
        value: String,
    },
}

impl MailQuery {
    /// Parses a mail query from the DSL.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] for an unbalanced quote, an empty operator value, or
    /// a malformed `before:`/`after:` date or `has_attachment:` boolean.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut query = MailQuery::default();
        for token in split_tokens(input)? {
            match classify(&token, MailOp::parse) {
                Term::Free(text) => push_free(&mut query.text.unscoped, text),
                Term::Operator { op, key, value } => apply_mail(&mut query, op, &key, value)?,
            }
        }
        Ok(query)
    }
}

impl FromStr for MailQuery {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl CalendarQuery {
    /// Parses a calendar query from the DSL.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] for an unbalanced quote, an empty operator value, or
    /// a malformed `before:`/`after:` date or `has_conference:` boolean.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut query = CalendarQuery::default();
        for token in split_tokens(input)? {
            match classify(&token, CalendarOp::parse) {
                Term::Free(text) => push_free(&mut query.text.unscoped, text),
                Term::Operator { op, key, value } => apply_calendar(&mut query, op, &key, value)?,
            }
        }
        Ok(query)
    }
}

impl FromStr for CalendarQuery {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The mail operators, the single source of truth for which keywords parse as
/// filters versus free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailOp {
    From,
    To,
    Cc,
    Mailbox,
    Label,
    Keyword,
    Subject,
    After,
    Before,
    HasAttachment,
}

impl MailOp {
    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "from" => Self::From,
            "to" => Self::To,
            "cc" => Self::Cc,
            "mailbox" => Self::Mailbox,
            "label" => Self::Label,
            "keyword" => Self::Keyword,
            "subject" => Self::Subject,
            "after" => Self::After,
            "before" => Self::Before,
            "has_attachment" => Self::HasAttachment,
            _ => return None,
        })
    }
}

/// The calendar operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalendarOp {
    Calendar,
    Attendee,
    Organizer,
    Rsvp,
    Location,
    After,
    Before,
    HasConference,
}

impl CalendarOp {
    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "calendar" => Self::Calendar,
            "attendee" => Self::Attendee,
            "organizer" => Self::Organizer,
            "rsvp" => Self::Rsvp,
            "location" => Self::Location,
            "after" => Self::After,
            "before" => Self::Before,
            "has_conference" => Self::HasConference,
            _ => return None,
        })
    }
}

/// A classified token: a typed operator with its value, or a free-text term.
enum Term<Op> {
    Free(String),
    Operator { op: Op, key: String, value: String },
}

/// Splits the input into tokens, honoring double-quoted spans (which may contain
/// spaces). Surrounding quotes are kept on the token and stripped at
/// interpretation time by [`unquote`].
///
/// Returns [`ParseError::UnbalancedQuote`] if a quote is left open.
fn split_tokens(input: &str) -> Result<Vec<String>, ParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in input.chars() {
        if ch == '"' {
            in_quote = !in_quote;
            current.push(ch);
        } else if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if in_quote {
        return Err(ParseError::UnbalancedQuote);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Classifies one token. It is an operator only when the text before the first
/// (unquoted) colon is a known keyword; otherwise the whole token is free text.
fn classify<Op>(token: &str, parse_op: impl Fn(&str) -> Option<Op>) -> Term<Op> {
    if let Some(colon) = token.find(':') {
        let head = &token[..colon];
        // A quote in the head means the colon was inside a quoted span (e.g.
        // `"a:b"`), so this is not a `keyword:value` operator.
        if !head.contains('"') {
            let key = head.to_ascii_lowercase();
            if let Some(op) = parse_op(&key) {
                return Term::Operator {
                    op,
                    key,
                    value: unquote(&token[colon + 1..]),
                };
            }
        }
    }
    Term::Free(unquote(token))
}

/// Strips one layer of surrounding double quotes, if present.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

/// Pushes a free-text term unless it is empty (e.g. an empty quoted string).
fn push_free(terms: &mut Vec<String>, text: String) {
    if !text.is_empty() {
        terms.push(text);
    }
}

fn apply_mail(
    query: &mut MailQuery,
    op: MailOp,
    key: &str,
    value: String,
) -> Result<(), ParseError> {
    if value.is_empty() {
        return Err(ParseError::EmptyValue {
            operator: key.to_owned(),
        });
    }
    match op {
        MailOp::From => query.from.push(value),
        MailOp::To => query.to.push(value),
        MailOp::Cc => query.cc.push(value),
        MailOp::Mailbox => query.mailbox.push(value),
        MailOp::Label => query.label.push(value),
        MailOp::Keyword => query.keyword.push(value),
        MailOp::Subject => query.text.scoped.push(ScopedTerm {
            field: TextField::Subject,
            text: value,
        }),
        MailOp::After => query.after = Some(parse_date(key, &value)?),
        MailOp::Before => query.before = Some(parse_date(key, &value)?),
        MailOp::HasAttachment => query.has_attachment = Some(parse_bool(key, &value)?),
    }
    Ok(())
}

fn apply_calendar(
    query: &mut CalendarQuery,
    op: CalendarOp,
    key: &str,
    value: String,
) -> Result<(), ParseError> {
    if value.is_empty() {
        return Err(ParseError::EmptyValue {
            operator: key.to_owned(),
        });
    }
    match op {
        CalendarOp::Calendar => query.calendar.push(value),
        CalendarOp::Attendee => query.attendee.push(value),
        CalendarOp::Organizer => query.organizer.push(value),
        CalendarOp::Rsvp => query.rsvp.push(ParticipationStatus::from_wire(&value)),
        CalendarOp::Location => query.text.scoped.push(ScopedTerm {
            field: TextField::Location,
            text: value,
        }),
        CalendarOp::After => query.after = Some(parse_date(key, &value)?),
        CalendarOp::Before => query.before = Some(parse_date(key, &value)?),
        CalendarOp::HasConference => query.has_conference = Some(parse_bool(key, &value)?),
    }
    Ok(())
}

fn parse_date(operator: &str, value: &str) -> Result<CalendarDate, ParseError> {
    value.parse().map_err(|_| ParseError::InvalidDate {
        operator: operator.to_owned(),
        value: value.to_owned(),
    })
}

fn parse_bool(operator: &str, value: &str) -> Result<bool, ParseError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(ParseError::InvalidBool {
            operator: operator.to_owned(),
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> CalendarDate {
        s.parse().unwrap()
    }

    #[test]
    fn empty_input_is_an_empty_query() {
        assert_eq!(MailQuery::parse("").unwrap(), MailQuery::default());
        assert_eq!(MailQuery::parse("   ").unwrap(), MailQuery::default());
        assert_eq!(CalendarQuery::parse("").unwrap(), CalendarQuery::default());
    }

    #[test]
    fn bare_words_are_free_text_terms() {
        let q = MailQuery::parse("quarterly report").unwrap();
        assert_eq!(q.text.unscoped, vec!["quarterly", "report"]);
        assert!(q.text.scoped.is_empty());
    }

    #[test]
    fn a_quoted_phrase_is_one_free_term() {
        let q = MailQuery::parse("\"quarterly report\" urgent").unwrap();
        assert_eq!(q.text.unscoped, vec!["quarterly report", "urgent"]);
    }

    #[test]
    fn empty_quoted_free_text_is_dropped() {
        let q = MailQuery::parse("\"\" hi").unwrap();
        assert_eq!(q.text.unscoped, vec!["hi"]);
    }

    #[test]
    fn each_mail_operator_parses() {
        let q = MailQuery::parse(
            "from:alice@x.com to:bob@x.com cc:carol@x.com mailbox:inbox label:work keyword:$flagged",
        )
        .unwrap();
        assert_eq!(q.from, vec!["alice@x.com"]);
        assert_eq!(q.to, vec!["bob@x.com"]);
        assert_eq!(q.cc, vec!["carol@x.com"]);
        assert_eq!(q.mailbox, vec!["inbox"]);
        assert_eq!(q.label, vec!["work"]);
        assert_eq!(q.keyword, vec!["$flagged"]);
    }

    #[test]
    fn mail_subject_is_scoped_text_not_a_filter() {
        let q = MailQuery::parse("subject:invoice").unwrap();
        assert!(q.text.unscoped.is_empty());
        assert_eq!(
            q.text.scoped,
            vec![ScopedTerm {
                field: TextField::Subject,
                text: "invoice".into(),
            }]
        );
    }

    #[test]
    fn mail_dates_and_attachment_scalar() {
        let q = MailQuery::parse("after:2026-01-01 before:2026-04-01 has_attachment:true").unwrap();
        assert_eq!(q.after, Some(date("2026-01-01")));
        assert_eq!(q.before, Some(date("2026-04-01")));
        assert_eq!(q.has_attachment, Some(true));
    }

    #[test]
    fn boolean_spellings() {
        for (spelling, expected) in [
            ("true", true),
            ("YES", true),
            ("1", true),
            ("false", false),
            ("no", false),
            ("0", false),
        ] {
            let q = MailQuery::parse(&format!("has_attachment:{spelling}")).unwrap();
            assert_eq!(q.has_attachment, Some(expected), "spelling {spelling:?}");
        }
    }

    #[test]
    fn unknown_operators_are_free_text() {
        // The keyword is not known, so the whole token (colon and all) is text.
        let q = MailQuery::parse("fromm:x foo:bar plain").unwrap();
        assert_eq!(q.text.unscoped, vec!["fromm:x", "foo:bar", "plain"]);
        assert!(q.from.is_empty());
    }

    #[test]
    fn urls_and_ratios_are_not_operators() {
        let q = MailQuery::parse("http://example.com 3:1 see").unwrap();
        assert_eq!(q.text.unscoped, vec!["http://example.com", "3:1", "see"]);
    }

    #[test]
    fn quoted_operator_values_keep_spaces() {
        let q = MailQuery::parse("subject:\"quarterly report\" from:\"a b@x.com\"").unwrap();
        assert_eq!(
            q.text.scoped,
            vec![ScopedTerm {
                field: TextField::Subject,
                text: "quarterly report".into(),
            }]
        );
        assert_eq!(q.from, vec!["a b@x.com"]);
    }

    #[test]
    fn quoted_value_with_internal_colon_is_text_not_operator() {
        // `"a:b"` — the colon is inside quotes, so this is a free-text phrase.
        let q = MailQuery::parse("\"a:b\"").unwrap();
        assert_eq!(q.text.unscoped, vec!["a:b"]);
    }

    #[test]
    fn operator_keywords_are_case_insensitive() {
        let q = MailQuery::parse("From:a SUBJECT:b").unwrap();
        assert_eq!(q.from, vec!["a"]);
        assert_eq!(q.text.scoped[0].text, "b");
    }

    #[test]
    fn repeated_operators_accumulate() {
        let q = MailQuery::parse("from:a@x.com from:b@x.com").unwrap();
        assert_eq!(q.from, vec!["a@x.com", "b@x.com"]);
    }

    #[test]
    fn from_str_impl_parses() {
        let q: MailQuery = "from:a@x.com".parse().unwrap();
        assert_eq!(q.from, vec!["a@x.com"]);
        let c: CalendarQuery = "calendar:work".parse().unwrap();
        assert_eq!(c.calendar, vec!["work"]);
    }

    #[test]
    fn mail_errors() {
        assert_eq!(
            MailQuery::parse("from:"),
            Err(ParseError::EmptyValue {
                operator: "from".into()
            })
        );
        assert_eq!(
            MailQuery::parse("before:not-a-date"),
            Err(ParseError::InvalidDate {
                operator: "before".into(),
                value: "not-a-date".into(),
            })
        );
        assert_eq!(
            MailQuery::parse("has_attachment:maybe"),
            Err(ParseError::InvalidBool {
                operator: "has_attachment".into(),
                value: "maybe".into(),
            })
        );
        assert_eq!(
            MailQuery::parse("subject:\"unterminated"),
            Err(ParseError::UnbalancedQuote)
        );
    }

    #[test]
    fn each_calendar_operator_parses() {
        let q = CalendarQuery::parse(
            "calendar:work attendee:carol@x.com organizer:dave@x.com location:\"room 4\" \
             after:2026-06-01 before:2026-07-01 has_conference:true",
        )
        .unwrap();
        assert_eq!(q.calendar, vec!["work"]);
        assert_eq!(q.attendee, vec!["carol@x.com"]);
        assert_eq!(q.organizer, vec!["dave@x.com"]);
        assert_eq!(
            q.text.scoped,
            vec![ScopedTerm {
                field: TextField::Location,
                text: "room 4".into(),
            }]
        );
        assert_eq!(q.after, Some(date("2026-06-01")));
        assert_eq!(q.before, Some(date("2026-07-01")));
        assert_eq!(q.has_conference, Some(true));
    }

    #[test]
    fn rsvp_maps_to_participation_status_and_preserves_unknown() {
        let q = CalendarQuery::parse("rsvp:accepted rsvp:bogus").unwrap();
        assert_eq!(
            q.rsvp,
            vec![
                ParticipationStatus::Accepted,
                ParticipationStatus::Other("bogus".into()),
            ]
        );
    }

    #[test]
    fn calendar_operators_reject_empty_values() {
        assert_eq!(
            CalendarQuery::parse("calendar:"),
            Err(ParseError::EmptyValue {
                operator: "calendar".into()
            })
        );
    }

    #[test]
    fn calendar_has_conference_error_is_distinct() {
        assert_eq!(
            CalendarQuery::parse("has_conference:nope"),
            Err(ParseError::InvalidBool {
                operator: "has_conference".into(),
                value: "nope".into(),
            })
        );
    }

    #[test]
    fn operators_are_domain_specific() {
        // `mailbox:` is mail-only, so in a calendar query it is free text...
        let cal = CalendarQuery::parse("mailbox:inbox subject:hi").unwrap();
        assert_eq!(cal.text.unscoped, vec!["mailbox:inbox", "subject:hi"]);
        // ...and `calendar:`/`location:` are calendar-only, so in a mail query
        // they are free text.
        let mail = MailQuery::parse("calendar:work location:room").unwrap();
        assert_eq!(mail.text.unscoped, vec!["calendar:work", "location:room"]);
    }
}
