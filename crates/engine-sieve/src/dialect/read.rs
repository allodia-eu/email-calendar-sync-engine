//! A script's rules, read segment by segment.

use std::collections::BTreeSet;

use engine_core::rules::{Expansion, MailRule, RawRule, RuleName, RuleOrigin, RuleSyntax};

use super::{actions, cursor::Cursor, facts, matching};
use crate::{
    lex::{Token, TokenKind, tokenize},
    segment::{Segment, SegmentKind},
};

/// The prefix of the comment line that names a rule, as Roundcube writes it.
const NAME_PREFIX: &str = "# rule:[";

/// The name the comments before a rule give it: the last `# rule:[name]` line, as Roundcube
/// reads it (an earlier one names nothing).
pub(super) fn name(leading: &str) -> Option<RuleName> {
    leading
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(NAME_PREFIX)?.strip_suffix(']'))
        .and_then(|name| RuleName::new(name).ok())
}

/// What the reader knows of the script a segment is read from.
pub(super) struct Script {
    /// Whether values read as literal text ([`super::literal`]).
    pub(super) literal: bool,
    /// Every extension the script's `require`s name.
    pub(super) requires: BTreeSet<String>,
    /// The script's [`facts::fingerprint`].
    pub(super) fingerprint: u64,
}

/// The comments before a rule other than its `# rule:[…]` lines: what a rewritten rule keeps.
pub(super) fn without_names(leading: &str) -> String {
    leading
        .split_inclusive('\n')
        .filter(|line| !line.starts_with(NAME_PREFIX))
        .collect()
}

/// The rule segment `index` holds: typed where the dialect reads it, otherwise carried whole as
/// its exact text. `None` for a `require` or the trailing segment, which are no rules.
pub(super) fn rule(segment: &Segment<'_>, index: usize, script: &Script) -> Option<MailRule> {
    if segment.kind() != SegmentKind::Command {
        return None;
    }
    let rule = match typed(segment.tokens(), script.literal) {
        Some((true, rule)) => rule,
        Some((false, rule)) => rule.disabled(),
        None => return Some(whole(segment, index, script)),
    };
    Some(placed(rule, segment, index, script))
}

/// The rule segment `index` holds, carried whole as its exact text, whether or not the dialect
/// could read it typed.
pub(super) fn whole(segment: &Segment<'_>, index: usize, script: &Script) -> MailRule {
    let named = name(segment.leading());
    let rule = MailRule::opaque(raw(segment, Some(script), named));
    placed(rule, segment, index, script)
}

/// `rule` with the name segment `index` gives it and its origin there.
fn placed(rule: MailRule, segment: &Segment<'_>, index: usize, script: &Script) -> MailRule {
    let rule = match name(segment.leading()) {
        Some(name) => rule.named(name),
        None => rule,
    };
    rule.with_origin(RuleOrigin::ScriptSegment {
        script: script.fingerprint,
        index,
    })
}

/// A segment carried whole, with what its tokens say it needs and does. `script` is the script it
/// was read from; `None` for a command a host wrote, with no script to fall back to.
fn raw(segment: &Segment<'_>, script: Option<&Script>, name: Option<RuleName>) -> RawRule {
    let tokens = segment.tokens();
    let mut requires = facts::requirements(tokens, script.map(|script| &script.requires));
    let mut raw = RawRule::new(RuleSyntax::Sieve, segment.text());
    if facts::holds_expansion(tokens) {
        let expanded = script.is_some_and(|script| !script.literal);
        if expanded && let (Some(requires), Some(script)) = (&mut requires, script) {
            // What made the source expand it is what it needs to mean the same.
            for extension in ["variables", "encoded-character"] {
                if script.requires.contains(extension) {
                    requires.insert(extension.to_owned());
                }
            }
        }
        raw = raw.holding(if expanded {
            Expansion::Expanded
        } else {
            Expansion::Literal
        });
    }
    if let Some(requires) = requires {
        raw = raw.requiring(requires);
    }
    if !facts::sends_mail(tokens) {
        raw = raw.sending_no_mail();
    }
    if !facts::names_folders_by_id(tokens) {
        raw = raw.naming_no_folder_by_id();
    }
    if !facts::includes_scripts(tokens) {
        raw = raw.including_no_script();
    }
    match name {
        Some(name) => raw.named(name),
        None => raw,
    }
}

/// A command written by the engine itself and carried whole (the `include` of a reply
/// script): what [`read_rules`](super::read_rules) would make of it as a script of its own. A
/// token the tables do not know leaves what it requires unknown.
pub(super) fn carried(text: &str) -> Option<RawRule> {
    let segments = crate::segment(text).ok()?;
    let [segment] = segments.as_slice() else {
        return None;
    };
    (segment.kind() == SegmentKind::Command).then(|| raw(segment, None, name(segment.leading())))
}

/// An `if` the dialect reads: whether it is enabled, and the rule, enabled, unnamed.
fn typed(tokens: &[Token<'_>], literal: bool) -> Option<(bool, MailRule)> {
    let (first, rest) = tokens.split_first()?;
    if !first.is_identifier("if") {
        return None;
    }
    // A disabled rule: `if false # <test>`, the test in the comment that ends the line.
    let significant: Vec<(usize, &Token<'_>)> = rest
        .iter()
        .enumerate()
        .filter(|(_, token)| token.kind() != TokenKind::Whitespace)
        .collect();
    let (enabled, when, block) = match significant.as_slice() {
        [(_, word), (at, comment), ..]
            if word.is_identifier("false") && comment.kind() == TokenKind::HashComment =>
        {
            let test = comment.text()[1..]
                .strip_prefix(' ')
                .unwrap_or(&comment.text()[1..]);
            let test_tokens = tokenize(test).ok()?;
            let mut cursor = Cursor::new(&test_tokens, literal);
            let when = matching::read(&mut cursor)?;
            if !cursor.done() {
                return None;
            }
            (false, when, &rest[at + 1..])
        }
        _ => {
            let mut cursor = Cursor::new(rest, literal);
            let (enabled, when) = matching::read_rule_test(&mut cursor)?;
            let block = rest.len() - remaining(&cursor, rest)?;
            (enabled, when, &rest[block..])
        }
    };
    let mut cursor = Cursor::new(block, literal);
    let (actions, stop) = actions::read(&mut cursor)?;
    if !cursor.done() {
        return None;
    }
    let rule = MailRule::typed(when, actions);
    Some((enabled, if stop { rule.stopping() } else { rule }))
}

/// How many of `tokens` the cursor has not read yet, counting trivia: the block starts at the
/// first significant token it has not read.
fn remaining(cursor: &Cursor<'_, '_>, tokens: &[Token<'_>]) -> Option<usize> {
    let next = cursor.peek()?;
    let at = tokens
        .iter()
        .position(|token| token.start() == next.start())?;
    Some(tokens.len() - at)
}
