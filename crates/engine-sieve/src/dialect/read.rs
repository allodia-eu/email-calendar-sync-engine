//! A script's rules, read segment by segment.

use engine_core::rules::{MailRule, RawRule, RuleName, RuleOrigin, RuleSyntax};

use super::{actions, cursor::Cursor, matching};
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
        .map(RuleName::new)
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
pub(super) fn rule(segment: &Segment<'_>, index: usize, literal: bool) -> Option<MailRule> {
    if segment.kind() != SegmentKind::Command {
        return None;
    }
    let rule = match typed(segment.tokens(), literal) {
        Some((true, rule)) => rule,
        Some((false, rule)) => rule.disabled(),
        None => MailRule::opaque(RawRule::new(RuleSyntax::Sieve, segment.text())),
    };
    let rule = match name(segment.leading()) {
        Some(name) => rule.named(name),
        None => rule,
    };
    Some(rule.with_origin(RuleOrigin::ScriptSegment(index)))
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
            let when = matching::read(&mut cursor)?;
            let block = rest.len() - remaining(&cursor, rest)?;
            (true, when, &rest[block..])
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
