//! Unit tests for the search DSL parser.

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
