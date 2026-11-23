//! A rule list written over the script it was read from.

use std::collections::BTreeSet;

use engine_core::rules::{MailRule, RuleBody, RuleOrigin, RuleSyntax};

use super::{DialectError, read, render};
use crate::{
    require,
    segment::{Segment, SegmentKind, segment},
    string,
};

/// `rules` as a script, copying from `base` every rule that is as it was read, byte for byte.
pub(super) fn write(base: &str, rules: &[MailRule]) -> Result<String, DialectError> {
    let segments = segment(base).map_err(DialectError::unsplittable)?;
    let script = super::script(base, &segments).map_err(DialectError::unsplittable)?;
    let mut needs = render::Extensions::new();
    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut pieces: Vec<(bool, String)> = Vec::with_capacity(rules.len());
    for (index, rule) in rules.iter().enumerate() {
        // An origin names a segment of the script it was read from, and of no other.
        let origin = match rule.origin {
            Some(RuleOrigin::ScriptSegment {
                script: from,
                index: at,
            }) if from == script.fingerprint => segments.get(at).map(|segment| (at, segment)),
            _ => None,
        };
        if let Some((at, segment)) = origin
            && read::rule(segment, at, &script).as_ref() == Some(rule)
        {
            pieces.push((false, segment.text().to_owned()));
            continue;
        }
        let text = match &rule.body {
            // Carried whole, its text holds its name and runs it: neither can change.
            RuleBody::Opaque(raw)
                if raw.syntax() == RuleSyntax::Sieve
                    && rule.enabled
                    && rule.name.as_ref() == raw.name() =>
            {
                let requires = raw.requires().ok_or(DialectError::unrenderable(index))?;
                needed.extend(requires.iter().cloned());
                raw.text().to_owned()
            }
            RuleBody::Typed(_) => {
                let kept = origin.map_or_else(String::new, |(_, segment)| {
                    read::without_names(segment.leading())
                });
                let text = render::rule(rule, &kept, &mut needs)
                    .map_err(|_| DialectError::unrenderable(index))?;
                if !script.literal && expands(&text) {
                    return Err(DialectError::variable(index));
                }
                text
            }
            _ => return Err(DialectError::unrenderable(index)),
        };
        pieces.push((true, text));
    }
    needed.extend(needs.into_iter().map(str::to_owned));
    let merged = require::merged(&segments, &needed).map_err(DialectError::require)?;

    let mut out = String::with_capacity(base.len());
    let requires: Vec<&Segment<'_>> = segments
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Require)
        .collect();
    match (merged, requires.first()) {
        (None, _) => requires
            .iter()
            .for_each(|segment| out.push_str(segment.text())),
        (Some(new), Some(first)) => {
            out.push_str(first.leading());
            out.push_str(&new);
        }
        (Some(new), None) => out.push_str(&new),
    }
    for (rendered, piece) in pieces {
        // A rule written here starts on a line of its own, so its name comment is its own.
        if rendered && !out.is_empty() && !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        out.push_str(&piece);
    }
    if let Some(trailing) = segments
        .last()
        .filter(|segment| segment.kind() == SegmentKind::Trailing)
    {
        out.push_str(trailing.text());
    }
    Ok(out)
}

/// Whether a written rule holds a value with `${`, which a script that requires `variables` or
/// `encoded-character` would expand rather than read as written.
fn expands(text: &str) -> bool {
    crate::tokenize(text).is_ok_and(|tokens| {
        tokens
            .iter()
            .filter_map(string::value)
            .any(|value| value.contains("${"))
    })
}
