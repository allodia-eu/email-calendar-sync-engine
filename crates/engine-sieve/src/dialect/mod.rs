//! The rule dialect: a Sieve script read as the neutral rule list, and the list written back.
//!
//! The engine reads and writes the dialect Roundcube's managesieve plugin writes
//! (`docs/design/filters-and-out-of-office.md`, decision 3), so a user who also uses Roundcube
//! sees and edits the same named rules there. Its bytes are pinned from the plugin's own output
//! (`tests/fixtures/roundcube/`).
//!
//! - [`read_rules`] reads each top-level command but the out-of-office reply's ([`reply`]) as one
//!   rule, in order: an `if` the dialect writes as a typed rule, anything else (a hand-written
//!   test, `keep`, `vacation`, `include`, a `set`) as a rule carried whole, its exact text. A
//!   `require` and what trails the last command are no rules. Each rule records the segment it came
//!   from ([`RuleOrigin::ScriptSegment`](engine_core::rules::RuleOrigin::ScriptSegment)).
//! - [`write_rules`] writes a rule list over the script it was read from: a rule that is still what
//!   its segment reads as is copied byte for byte, comments and layout included; a changed or new
//!   rule is written as Roundcube writes one, keeping any comments that led it; a rule left out is
//!   gone. The reply's rule stays where it was among them, byte for byte. One `require` names what
//!   the written rules need, added to what the script named, and is the only other line that
//!   changes.
//! - [`reply`] reads and writes the out-of-office reply where it is a rule of the script, as the
//!   plugin's vacation engine writes it.
//! - [`include`] is a rules script's `include` of a server's reply script, and [`scripts`] which of
//!   an account's scripts holds the rules: one answer for JMAP and ManageSieve.
//! - [`vocabulary`] says what the writer can write for an account, from its server's extensions,
//!   for [`plan_rules`](engine_core::rules::plan_rules).
//!
//! Two of Roundcube's conventions apply. A rule's name is the last `# rule:[name]` line before
//! it (an earlier one names nothing, and a rewritten rule keeps the other comments but not
//! those lines). A `text:` block's value is read without the line end before its closing
//! period, and written with one added, though RFC 5228 §2.4.2 makes that line end part of the
//! value ([`crate::string::value`] keeps it).
//!
//! Where the script requires `variables` or `encoded-character`, a value holding `${` is not
//! literal text, and a rule with one is carried whole.

mod actions;
mod cursor;
mod facts;
pub mod include;
mod matching;
mod read;
mod render;
pub mod reply;
mod reply_read;
mod reply_render;
pub mod scripts;
mod vocabulary;
mod write;

use core::fmt;

use engine_core::rules::{MailRule, RawRule, RuleSyntax};
pub use vocabulary::vocabulary;

use crate::{SieveError, SieveErrorKind, require, segment::Segment};

/// Each rule `script` holds, in order ([module docs](self)).
///
/// A script that cannot be split at its commands reads as one rule carried whole, with no
/// origin: a host can show it is there, and [`write_rules`] refuses to write over it.
pub fn read_rules(script: &str) -> Vec<MailRule> {
    let Ok(segments) = crate::segment(script) else {
        return whole(script);
    };
    let Ok(context) = self::script(script, &segments) else {
        return whole(script);
    };
    // The out-of-office reply is a rule of the script, but no rule of the user's (`reply`).
    let reply = reply_read::find(script, &segments, context.literal).map(|(at, _)| at);
    segments
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != reply)
        .filter_map(|(index, segment)| read::rule(segment, index, &context))
        .collect()
}

/// A script the reader cannot split, as one rule: what it requires and whether it sends mail
/// are not known, so the plan carries it nowhere and an import never.
fn whole(script: &str) -> Vec<MailRule> {
    if script.is_empty() {
        return Vec::new();
    }
    vec![MailRule::opaque(RawRule::new(RuleSyntax::Sieve, script))]
}

/// `text` as one rule carried whole, with what its tokens say it needs and does: for a command
/// a host writes itself (the `include` of a script), so the writer requires what it needs and
/// the plan knows whether it sends mail. `None` where `text` is not one command.
pub fn carried(text: &str) -> Option<RawRule> {
    read::carried(text)
}

/// What the reader knows of `text`, split into `segments`.
fn script(text: &str, segments: &[Segment<'_>]) -> Result<read::Script, SieveError> {
    Ok(read::Script {
        literal: literal(segments)?,
        requires: require::required(segments)?,
        fingerprint: facts::fingerprint(text),
    })
}

/// `rules` written as a script over `base`, the script they were read from ([module
/// docs](self)); `base` is empty for an account that has none.
///
/// # Errors
///
/// A [`DialectError`]: `base` cannot be split at its commands, a rule holds what the writer
/// cannot write, or the `require` cannot be written. It refuses only what a rule
/// [`plan_rules`](engine_core::rules::plan_rules) kept under this account's [`vocabulary`]
/// never holds, so the plan runs first: a rule carried whole that was renamed, disabled, or of
/// unknown needs; a typed part the vocabulary does not list; a value with `${` over a script
/// that would expand it.
pub fn write_rules(base: &str, rules: &[MailRule]) -> Result<String, DialectError> {
    write::write(base, rules)
}

/// Whether values in the script read as literal text: not where it requires an extension that
/// expands `${…}`.
fn literal(segments: &[Segment<'_>]) -> Result<bool, SieveError> {
    let required = require::required(segments)?;
    Ok(!required.contains("variables") && !required.contains("encoded-character"))
}

/// Why a rule list could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialectError {
    kind: DialectErrorKind,
}

/// What [`DialectError`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialectErrorKind {
    /// The script written over cannot be split at its commands, or names a malformed `require`.
    Unsplittable(SieveErrorKind),
    /// The rule at this index holds what the writer cannot write.
    Unrenderable {
        /// The rule's index in the list handed in.
        rule: usize,
    },
    /// The `require` the rules need cannot be written.
    Require(SieveErrorKind),
    /// The rule at this index holds a value with `${` over a script that requires `variables`
    /// or `encoded-character`, which would expand it. The writer does not escape it.
    Variable {
        /// The rule's index in the list handed in.
        rule: usize,
    },
    /// The out-of-office reply cannot be written as a rule of the script, for this reason
    /// ([`reply::write_reply`]).
    Reply(reply::ReplyRefusal),
}

impl DialectError {
    fn unsplittable(err: SieveError) -> Self {
        Self {
            kind: DialectErrorKind::Unsplittable(err.kind()),
        }
    }

    fn unrenderable(rule: usize) -> Self {
        Self {
            kind: DialectErrorKind::Unrenderable { rule },
        }
    }

    fn variable(rule: usize) -> Self {
        Self {
            kind: DialectErrorKind::Variable { rule },
        }
    }

    fn reply(reason: reply::ReplyRefusal) -> Self {
        Self {
            kind: DialectErrorKind::Reply(reason),
        }
    }

    fn require(err: SieveError) -> Self {
        Self {
            kind: DialectErrorKind::Require(err.kind()),
        }
    }

    /// What was found.
    pub fn kind(&self) -> DialectErrorKind {
        self.kind
    }
}

impl fmt::Display for DialectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            DialectErrorKind::Unsplittable(kind) => {
                write!(f, "the script cannot be edited rule by rule ({kind:?})")
            }
            DialectErrorKind::Unrenderable { rule } => {
                write!(f, "rule {rule} holds what the Sieve writer cannot write")
            }
            DialectErrorKind::Variable { rule } => write!(
                f,
                "rule {rule} holds a value the script's variables would expand"
            ),
            DialectErrorKind::Require(kind) => {
                write!(f, "the script's require cannot be written ({kind:?})")
            }
            DialectErrorKind::Reply(reason) => write!(
                f,
                "the out-of-office reply cannot be written as a Sieve rule ({reason:?})"
            ),
        }
    }
}

impl std::error::Error for DialectError {}
