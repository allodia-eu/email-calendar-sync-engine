//! Unit tests for FTS match-string construction and term quoting.

use engine_search::ScopedTerm;

use super::*;

/// Each term becomes a quoted-phrase prefix query (`"term"*`); scoped terms
/// keep their column filter. This is the search-as-you-type form, so a typed
/// `allo` matches a stored `allodia`.
#[test]
fn fts_match_builds_prefix_phrases() {
    let text = TextQuery {
        unscoped: vec!["allo".into(), "bar".into()],
        scoped: vec![ScopedTerm {
            field: TextField::Subject,
            text: "allo".into(),
        }],
    };
    assert_eq!(
        fts_match(&text).as_deref(),
        Some(r#""allo"* "bar"* subject:"allo"*"#)
    );
}

#[test]
fn fts_match_is_none_for_empty_text() {
    assert_eq!(fts_match(&TextQuery::default()), None);
}

/// Embedded quotes are doubled (injection-safe) and the `*` is appended after
/// the closing quote, not inside it.
#[test]
fn quote_term_doubles_quotes_then_appends_star() {
    assert_eq!(quote_term(r#"a"b"#), r#""a""b"*"#);
}
