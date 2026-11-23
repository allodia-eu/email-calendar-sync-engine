//! A rule's test, read into a [`Match`], and a [`Match`] written as a test.
//!
//! What the dialect reads is what it writes, and what Roundcube writes:
//!
//! | Match | Test |
//! |---|---|
//! | every message | `true` |
//! | one condition | the condition |
//! | conditions, all | `allof (c1, c2)` |
//! | conditions, any | `anyof (c1, c2)` |
//! | exceptions | `not e`, or `not e` items in an `allof` |
//! | any, with exceptions | `allof (anyof (c1, c2), not e)` |
//!
//! Any one condition is the same as all of one, so it reads back as [`MatchMode::All`]. Anything
//! else (an `anyof` holding a `not`, nested lists, a test this dialect does not know) is not a
//! rule the engine can show, and the rule is carried whole.

use std::collections::BTreeSet;

use engine_core::rules::{
    AddressField, AddressPart, Condition, HeaderField, Match, MatchMode, SizeBound, TextOp,
    TextTest,
};

use super::cursor::Cursor;
use crate::lex::TokenKind;

/// A test as written, before it is read as a [`Match`].
enum Test {
    True,
    False,
    Not(Box<Test>),
    AllOf(Vec<Test>),
    AnyOf(Vec<Test>),
    Basic(Condition),
}

/// Reads one test and the [`Match`] it stands for, or `None` for one this dialect does not write.
pub(super) fn read(cursor: &mut Cursor<'_, '_>) -> Option<Match> {
    to_match(test(cursor)?)
}

/// Reads an `if`'s test: whether the rule runs, and what it matches. `allof (false, <test>)` is a
/// disabled rule, as a hand-written script may say; the writer never writes it (`super::render`).
pub(super) fn read_rule_test(cursor: &mut Cursor<'_, '_>) -> Option<(bool, Match)> {
    match test(cursor)? {
        Test::AllOf(mut items)
            if matches!(items.first(), Some(Test::False)) && items.len() == 2 =>
        {
            Some((false, to_match(items.pop()?)?))
        }
        test => Some((true, to_match(test)?)),
    }
}

fn test(cursor: &mut Cursor<'_, '_>) -> Option<Test> {
    let name = cursor.next()?;
    if name.kind() != TokenKind::Identifier {
        return None;
    }
    match name.text().to_ascii_lowercase().as_str() {
        "true" => Some(Test::True),
        "false" => Some(Test::False),
        "not" => Some(Test::Not(Box::new(test(cursor)?))),
        "allof" => Some(Test::AllOf(list(cursor)?)),
        "anyof" => Some(Test::AnyOf(list(cursor)?)),
        "header" => header(cursor).map(Test::Basic),
        "address" => address(cursor).map(Test::Basic),
        "size" => size(cursor).map(Test::Basic),
        "body" => body(cursor).map(Test::Basic),
        _ => None,
    }
}

fn list(cursor: &mut Cursor<'_, '_>) -> Option<Vec<Test>> {
    cursor.eat(TokenKind::LeftParen)?;
    let mut tests = vec![test(cursor)?];
    while cursor.eat(TokenKind::Comma).is_some() {
        tests.push(test(cursor)?);
    }
    cursor.eat(TokenKind::RightParen)?;
    Some(tests)
}

fn to_match(test: Test) -> Option<Match> {
    match test {
        Test::False => None,
        Test::True => Some(Match::default()),
        Test::Basic(condition) => Some(Match::all(vec![condition])),
        Test::Not(inner) => Some(Match::all(Vec::new()).except(vec![basic(*inner)?])),
        Test::AnyOf(items) => {
            let conditions = items.into_iter().map(basic).collect::<Option<Vec<_>>>()?;
            Some(if conditions.len() == 1 {
                Match::all(conditions)
            } else {
                Match::any(conditions)
            })
        }
        Test::AllOf(items) => {
            let mut items = items.into_iter().peekable();
            let mut when = match items.peek() {
                Some(Test::AnyOf(_)) => to_match(items.next()?)?,
                _ => Match::default(),
            };
            let had_any = when.mode == MatchMode::Any || !when.conditions.is_empty();
            for item in items {
                match item {
                    Test::Not(inner) => when.except.push(basic(*inner)?),
                    Test::Basic(condition) if !had_any => when.conditions.push(condition),
                    _ => return None,
                }
            }
            Some(when)
        }
    }
}

fn basic(test: Test) -> Option<Condition> {
    match test {
        Test::Basic(condition) => Some(condition),
        _ => None,
    }
}

/// The match-type and comparator tags a text test may carry; `None` for any other tag, or a
/// comparator other than the default (RFC 5228 §2.7.3, `i;ascii-casemap`).
fn text_tag(cursor: &mut Cursor<'_, '_>, tag: &str, op: &mut Option<TextOp>) -> Option<()> {
    let found = match tag {
        "contains" => TextOp::Contains,
        "is" => TextOp::Is,
        "matches" => TextOp::Matches,
        "regex" => TextOp::Regex,
        "comparator" => {
            return (cursor.string()?.eq_ignore_ascii_case("i;ascii-casemap")).then_some(());
        }
        _ => return None,
    };
    op.replace(found).is_none().then_some(())
}

fn header(cursor: &mut Cursor<'_, '_>) -> Option<Condition> {
    let (mut op, mut mime, mut anychild) = (None, false, false);
    while let Some(tag) = cursor.tag() {
        match tag.as_str() {
            "mime" => mime = true,
            "anychild" => anychild = true,
            other => text_tag(cursor, other, &mut op)?,
        }
    }
    let names = cursor.strings()?;
    let keys = cursor.strings()?;
    if mime {
        // The one MIME test the dialect writes: an attachment, as `HasAttachment` is rendered.
        let attachment = anychild
            && op == Some(TextOp::Contains)
            && matches!(names.as_slice(), [name] if name.eq_ignore_ascii_case("content-disposition"))
            && keys == ["attachment"];
        return attachment.then_some(Condition::HasAttachment);
    }
    let [name] = <[String; 1]>::try_from(names).ok()?;
    Some(Condition::Header {
        field: HeaderField::named(name),
        test: TextTest::new(op.unwrap_or(TextOp::Is), keys).ok()?,
    })
}

fn address(cursor: &mut Cursor<'_, '_>) -> Option<Condition> {
    let (mut op, mut part) = (None, None);
    while let Some(tag) = cursor.tag() {
        let found = match tag.as_str() {
            "all" => AddressPart::All,
            "localpart" => AddressPart::LocalPart,
            "domain" => AddressPart::Domain,
            other => {
                text_tag(cursor, other, &mut op)?;
                continue;
            }
        };
        if part.replace(found).is_some() {
            return None;
        }
    }
    let fields: BTreeSet<String> = cursor
        .strings()?
        .into_iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let field = match fields
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["from"] => AddressField::From,
        ["to"] => AddressField::To,
        ["cc"] => AddressField::Cc,
        ["cc", "to"] => AddressField::ToOrCc,
        _ => return None,
    };
    let keys = cursor.strings()?;
    Some(Condition::Address {
        field,
        part: part.unwrap_or_default(),
        test: TextTest::new(op.unwrap_or(TextOp::Is), keys).ok()?,
    })
}

fn size(cursor: &mut Cursor<'_, '_>) -> Option<Condition> {
    let bound = match cursor.tag()?.as_str() {
        "over" => SizeBound::Over,
        "under" => SizeBound::Under,
        _ => return None,
    };
    Some(Condition::Size(bound(cursor.number()?)))
}

fn body(cursor: &mut Cursor<'_, '_>) -> Option<Condition> {
    let mut op = None;
    while let Some(tag) = cursor.tag() {
        if tag != "text" {
            text_tag(cursor, &tag, &mut op)?;
        }
    }
    let keys = cursor.strings()?;
    Some(Condition::Body(
        TextTest::new(op.unwrap_or(TextOp::Is), keys).ok()?,
    ))
}
