//! The part of Gmail's search language a filter is read into typed conditions from, and written
//! back in (`filters-and-out-of-office.md`, decision 4).
//!
//! A Gmail filter's criteria are search expressions: `from` and `subject` hold one each (`to`
//! too, read native: below), and `query` and `negatedQuery` hold whole searches. Only a small
//! subset is read typed, and only where every part of the expression is in it; anything else stays
//! whole, as a native condition that carries to Gmail unchanged and to no other target:
//!
//! - **A value** is a word (no space, quote, bracket or colon, not starting with `-` or `+`, and
//!   not one of the operators `OR`, `AND`, `AROUND`), or a phrase in double quotes ([`is_phrase`]).
//!   Several values are any-of: `a OR b`, `(a OR b)` or `{a b}`. `me` (the account's own
//!   addresses), bare or quoted, is not a value here.
//! - **A term** is `from:` or `subject:` with a value or a bracketed any-of, `has:attachment`, or
//!   `larger:` / `smaller:` with a size in bytes (no unit: what `10M` means is not pinned; a plain
//!   number is bytes, observed through the same search on mail).
//! - **A search** is terms joined by spaces, all of which must hold ([`conjunction`]), or terms
//!   joined by ` OR `, any one of which suffices ([`disjunction`]).
//!
//! Gmail matches these as a search does: by words and phrases, ignoring case
//! ([`TextOp::Words`]), `from:` over the sender's name and address ([`AddressPart::WithName`]).
//! Each is read as exactly that. `to:` matches every recipient, `To`, `Cc` and `Bcc`, which no
//! typed condition says, so it stays native. A subject that *contains* a value is written as a
//! word search, which matches less: approximated, and held as a condition only (an exception
//! that vetoes less makes the rule do more).

use engine_core::rules::{
    AddressField, AddressPart, Condition, HeaderField, SizeBound, TextOp, TextTest, is_phrase,
};

/// One term of the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Term {
    From(Vec<String>),
    Subject(Vec<String>),
    HasAttachment,
    Larger(u64),
    Smaller(u64),
}

impl Term {
    /// The condition this term is.
    pub(crate) fn condition(self) -> Option<Condition> {
        match self {
            Self::From(values) => Some(Condition::Address {
                field: AddressField::From,
                part: AddressPart::WithName,
                test: TextTest::new(TextOp::Words, values).ok()?,
            }),
            Self::Subject(values) => Some(Condition::Header {
                field: HeaderField::Subject,
                test: TextTest::new(TextOp::Words, values).ok()?,
            }),
            Self::HasAttachment => Some(Condition::HasAttachment),
            Self::Larger(bytes) => Some(Condition::Size(SizeBound::Over(bytes))),
            Self::Smaller(bytes) => Some(Condition::Size(SizeBound::Under(bytes))),
        }
    }

    /// The term `condition` is, where the subset has one: the sender with its name by words; the
    /// subject by words, or by `contains` as a word search (narrower); an attachment; a size
    /// bound.
    pub(crate) fn of(condition: &Condition) -> Option<Self> {
        match condition {
            Condition::Address {
                field: AddressField::From,
                part: AddressPart::WithName,
                test,
            } if test.op() == TextOp::Words => Some(Self::From(test.values().to_vec())),
            Condition::Header { field, test }
                if field.is_subject() && matches!(test.op(), TextOp::Words | TextOp::Contains) =>
            {
                Some(Self::Subject(test.values().to_vec()))
            }
            Condition::HasAttachment => Some(Self::HasAttachment),
            Condition::Size(SizeBound::Over(bytes)) => Some(Self::Larger(*bytes)),
            Condition::Size(SizeBound::Under(bytes)) => Some(Self::Smaller(*bytes)),
            _ => None,
        }
    }

    /// The term as search text, or `None` where a value cannot be written as one.
    pub(crate) fn render(&self) -> Option<String> {
        let values = |key: &str, values: &[String]| {
            Some(match values {
                [one] => format!("{key}:{}", atom(one)?),
                many => format!("{key}:({})", any_of(many)?),
            })
        };
        match self {
            Self::From(v) => values("from", v),
            Self::Subject(v) => values("subject", v),
            Self::HasAttachment => Some("has:attachment".to_owned()),
            Self::Larger(bytes) => Some(format!("larger:{bytes}")),
            Self::Smaller(bytes) => Some(format!("smaller:{bytes}")),
        }
    }
}

/// The values of a criteria field (`from`, `subject`): one value, or any-of.
pub(crate) fn field_values(text: &str) -> Option<Vec<String>> {
    let mut lexer = Lexer::new(text);
    let values = lexer.values(true)?;
    lexer.end().then_some(values)
}

/// The values written as a criteria field's text: one value, or `a OR b`.
pub(crate) fn render_values(values: &[String]) -> Option<String> {
    match values {
        [one] => atom(one),
        many => any_of(many),
    }
}

/// The terms of a search whose terms must all hold, or `None` where any part is not in the
/// subset.
pub(crate) fn conjunction(text: &str) -> Option<Vec<Term>> {
    let mut lexer = Lexer::new(text);
    let mut terms = vec![lexer.term()?];
    while !lexer.end() {
        lexer.space()?;
        terms.push(lexer.term()?);
    }
    Some(terms)
}

/// The terms of a search any one of which suffices (`a OR b`), or `None` where any part is not
/// in the subset. One term alone is a disjunction of one.
pub(crate) fn disjunction(text: &str) -> Option<Vec<Term>> {
    let mut lexer = Lexer::new(text);
    let mut terms = vec![lexer.term()?];
    while !lexer.end() {
        lexer.or()?;
        terms.push(lexer.term()?);
    }
    Some(terms)
}

fn any_of(values: &[String]) -> Option<String> {
    let atoms: Option<Vec<String>> = values.iter().map(|value| atom(value)).collect();
    Some(atoms?.join(" OR "))
}

/// One value as Gmail reads it back as that value: a word as it is, any other phrase quoted
/// ([`is_phrase`], what the plan holds the values to). Any other value cannot be.
fn atom(value: &str) -> Option<String> {
    if is_word(value) {
        return Some(value.to_owned());
    }
    is_phrase(value).then(|| format!("\"{value}\""))
}

/// Whether `text` is a value as it stands: no character the search language reads as syntax.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(['-', '+'])
        && !matches!(text, "OR" | "AND" | "AROUND")
        && !text.eq_ignore_ascii_case("me")
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"(){}:|".contains(c))
}

/// A reader over one search expression; every method answers `None` for what is not in the
/// subset.
struct Lexer<'a> {
    rest: &'a str,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text.trim() }
    }

    fn end(&self) -> bool {
        self.rest.is_empty()
    }

    fn eat(&mut self, prefix: &str) -> bool {
        match self.rest.strip_prefix(prefix) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    /// One or more spaces.
    fn space(&mut self) -> Option<()> {
        let trimmed = self.rest.trim_start_matches(' ');
        (trimmed.len() < self.rest.len()).then(|| self.rest = trimmed)
    }

    /// ` OR `, with any number of spaces around it.
    fn or(&mut self) -> Option<()> {
        self.space()?;
        self.eat("OR").then_some(())?;
        self.space()
    }

    /// A word, or a quoted phrase.
    fn atom(&mut self) -> Option<String> {
        if let Some(quoted) = self.rest.strip_prefix('"') {
            let close = quoted.find('"')?;
            let phrase = &quoted[..close];
            self.rest = &quoted[close + 1..];
            return is_phrase(phrase).then(|| phrase.to_owned());
        }
        let end = self
            .rest
            .find(|c: char| c.is_whitespace() || "\"(){}".contains(c))
            .unwrap_or(self.rest.len());
        let word = &self.rest[..end];
        self.rest = &self.rest[end..];
        is_word(word).then(|| word.to_owned())
    }

    /// A value or an any-of: `(a OR b)` or `{a b}`, or, where `bare` (a whole criteria field),
    /// also `a OR b`.
    fn values(&mut self, bare: bool) -> Option<Vec<String>> {
        if self.eat("(") {
            let mut values = vec![self.atom()?];
            while !self.eat(")") {
                self.or()?;
                values.push(self.atom()?);
            }
            return Some(values);
        }
        if self.eat("{") {
            let mut values = vec![self.atom()?];
            while !self.eat("}") {
                self.space()?;
                values.push(self.atom()?);
            }
            return Some(values);
        }
        let mut values = vec![self.atom()?];
        while bare && !self.end() {
            self.or()?;
            values.push(self.atom()?);
        }
        Some(values)
    }

    fn term(&mut self) -> Option<Term> {
        if self.eat("has:attachment") {
            return self.boundary().then_some(Term::HasAttachment);
        }
        for (key, make) in [
            ("larger:", Term::Larger as fn(u64) -> Term),
            ("smaller:", Term::Smaller),
        ] {
            if self.eat(key) {
                let end = self.rest.find(' ').unwrap_or(self.rest.len());
                let digits = &self.rest[..end];
                self.rest = &self.rest[end..];
                let all_digits = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
                return all_digits.then(|| digits.parse().ok().map(make))?;
            }
        }
        for (key, make) in [
            ("from:", Term::From as fn(Vec<String>) -> Term),
            ("subject:", Term::Subject),
        ] {
            if self.eat(key) {
                return self.values(false).map(make);
            }
        }
        None
    }

    /// Whether the term just read ends here: the end, or a space.
    fn boundary(&self) -> bool {
        self.rest.is_empty() || self.rest.starts_with(' ')
    }
}

#[cfg(test)]
#[path = "rules_query_tests.rs"]
mod tests;
