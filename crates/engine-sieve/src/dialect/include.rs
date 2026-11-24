//! A rules script's `include` of a server's own reply script (RFC 6609), as a host writes it.
//!
//! On a server that runs the out-of-office reply as a script of its own (RFC 9661 §4), an
//! account runs one script at a time, so the reply and the user's rules cannot both be active. A
//! host keeps the rules script active and has it run the reply's script with
//! `include :personal :optional "<reply>";` as its first command, while the reply is on: no rule's
//! `stop` or `discard` then keeps the reply from running, and `:optional` keeps a reply script
//! someone removed from failing delivery.
//!
//! That include is no rule of the user's, whichever protocol reads the script: [`without_include`]
//! leaves it out of the rules, and [`compose`] puts it back, first, on a write. One answer for
//! JMAP and ManageSieve alike.

use core::fmt;

use engine_core::rules::{MailRule, RuleBody};

use super::{DialectErrorKind, carried, read_rules, write_rules};
use crate::{
    TokenKind,
    segment::{Segment, SegmentKind, segment},
    string,
};

/// The include of the script `name`, as a host writes it, or `None` for a name no quoted string
/// holds.
#[must_use]
pub fn include_command(name: &str) -> Option<String> {
    Some(format!(
        "include :personal :optional {};\r\n",
        string::quoted(name)?
    ))
}

/// Whether `text` includes the script `name` at its top level.
#[must_use]
pub fn includes_script(text: &str, name: &str) -> bool {
    segment(text).is_ok_and(|segments| segments.iter().any(|s| is_include_of(s, name)))
}

/// The scripts `text` includes at its top level from the user's own (`:personal`, or no location,
/// which RFC 6609 §3.2 makes the same), in order.
#[must_use]
pub fn included_scripts(text: &str) -> Vec<String> {
    segment(text).map_or_else(
        |_| Vec::new(),
        |segments| segments.iter().filter_map(personal_include).collect(),
    )
}

/// The script a top-level `include` segment names, where it is one of the user's own scripts,
/// optionally `:optional` or `:once`. A `:global` include names another script of that name.
fn personal_include(segment: &Segment<'_>) -> Option<String> {
    let significant: Vec<_> = segment
        .tokens()
        .iter()
        .filter(|token| !token.kind().is_trivia())
        .collect();
    let [command, middle @ .., target, end] = significant.as_slice() else {
        return None;
    };
    let shaped = segment.kind() == SegmentKind::Command
        && command.is_identifier("include")
        && end.kind() == TokenKind::Semicolon
        && middle
            .iter()
            .all(|tag| tag.is_tag("personal") || tag.is_tag("optional") || tag.is_tag("once"));
    if shaped { string::value(target) } else { None }
}

fn is_include_of(segment: &Segment<'_>, name: &str) -> bool {
    personal_include(segment).as_deref() == Some(name)
}

/// Whether `rule` is the include of the script `name`: a rule that is exactly that one command,
/// never a whole script carried as one rule that holds it.
#[must_use]
pub fn is_include_rule(rule: &MailRule, name: &str) -> bool {
    match &rule.body {
        RuleBody::Opaque(raw) => segment(raw.text()).is_ok_and(|segments| {
            let commands: Vec<_> = segments
                .iter()
                .filter(|segment| segment.kind() != SegmentKind::Trailing)
                .collect();
            matches!(commands.as_slice(), [only] if is_include_of(only, name))
        }),
        _ => false,
    }
}

/// The user's rules in `text`: every rule ([`read_rules`]) but the include of the script `name`.
#[must_use]
pub fn without_include(text: &str, name: &str) -> Vec<MailRule> {
    read_rules(text)
        .into_iter()
        .filter(|rule| !is_include_rule(rule, name))
        .collect()
}

/// Why `rules` could not be written over a script: the dialect's refusal, with the index of the
/// rule it names counted among the rules handed in, not among those written with the include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeError {
    kind: DialectErrorKind,
    rule: Option<usize>,
}

impl ComposeError {
    /// What the dialect refused.
    #[must_use]
    pub fn kind(&self) -> DialectErrorKind {
        self.kind
    }

    /// The rule refused, as an index into the rules handed in, where one was.
    #[must_use]
    pub fn rule(&self) -> Option<usize> {
        self.rule
    }
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.kind, self.rule) {
            (DialectErrorKind::Unsplittable(_), _) => {
                f.write_str("the script cannot be edited rule by rule")
            }
            (DialectErrorKind::Variable { .. }, Some(rule)) => {
                write!(
                    f,
                    "rule {rule} holds a value the script's variables would expand"
                )
            }
            (_, Some(rule)) => write!(f, "rule {rule} holds what the Sieve writer cannot write"),
            (_, None) => f.write_str("the script's require cannot be written"),
        }
    }
}

impl std::error::Error for ComposeError {}

/// `rules` written over `base` ([`write_rules`]), with the include of the script `include` first
/// where there is one, and without any include of it otherwise. An include `base` already has is
/// copied as it was.
///
/// # Errors
///
/// A [`ComposeError`]: the dialect's refusal, its rule counted among `rules`.
pub fn compose(
    base: &str,
    rules: &[MailRule],
    include: Option<&str>,
) -> Result<String, ComposeError> {
    let mut list = Vec::with_capacity(rules.len() + 1);
    let unwritable = ComposeError {
        kind: DialectErrorKind::Unrenderable { rule: 0 },
        rule: None,
    };
    if let Some(name) = include {
        let existing = read_rules(base)
            .into_iter()
            .find(|rule| is_include_rule(rule, name));
        // A new include, carried with what it needs (`include`), so the writer requires it.
        let command = include_command(name).ok_or(unwritable)?;
        let new = carried(&command).map(MailRule::opaque);
        list.extend(existing.or(new));
    }
    let offset = list.len();
    let kept: Vec<usize> = (0..rules.len())
        .filter(|&i| include.is_none_or(|name| !is_include_rule(&rules[i], name)))
        .collect();
    list.extend(kept.iter().map(|&i| rules[i].clone()));
    write_rules(base, &list).map_err(|err| {
        let kind = err.kind();
        let rule = match kind {
            DialectErrorKind::Unrenderable { rule } | DialectErrorKind::Variable { rule } => rule
                .checked_sub(offset)
                .and_then(|at| kept.get(at).copied()),
            _ => None,
        };
        ComposeError { kind, rule }
    })
}
