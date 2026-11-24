//! What an importer asks of the dialect: a rule as a target writing it would hold it, and a rule
//! carried whole, its facts read from its own text.

use engine_core::rules::{MailRule, RawRule, RuleBody, RuleSyntax};

use super::{carried, read_rules, write_rules};

/// `rule` as a script this dialect writes holds it, read back, with no origin: what the account
/// will answer for it once written. `None` for a rule carried whole, or one the writer cannot
/// write as it stands.
///
/// The one answer to "does the account already hold this rule": a rule is compared in the form
/// the reader gives it, so whatever the writer and the reader make of it on the way (a forward
/// written as `redirect :copy`, two redirects alike read as one, a single condition of either
/// kind read as `All`, `INBOX` by path read as the inbox's role) is made of the rule handed in
/// too, rather than matched case by case.
#[must_use]
pub fn read_back(rule: &MailRule) -> Option<MailRule> {
    if !matches!(rule.body, RuleBody::Typed(_)) {
        return None;
    }
    let mut unplaced = rule.clone();
    unplaced.origin = None;
    let text = write_rules("", std::slice::from_ref(&unplaced)).ok()?;
    let mut read = read_rules(&text);
    if read.len() != 1 {
        return None;
    }
    let mut back = read.remove(0);
    back.origin = None;
    Some(back)
}

/// `rule`, where it is a Sieve rule carried whole, with what its text requires and does read from
/// the text itself ([`carried`]), whatever the host that built it said: the extensions, whether it
/// may send mail, names a folder by id or runs another script. A text that is not one command is
/// known to require nothing in particular, so no account stores it back. Any other rule is
/// returned as it is.
#[must_use]
pub fn facts_from_text(mut rule: MailRule) -> MailRule {
    let RuleBody::Opaque(raw) = &rule.body else {
        return rule;
    };
    if raw.syntax() != RuleSyntax::Sieve {
        return rule;
    }
    let fresh = carried(raw.text()).unwrap_or_else(|| RawRule::new(RuleSyntax::Sieve, raw.text()));
    rule.body = RuleBody::Opaque(fresh);
    rule
}
