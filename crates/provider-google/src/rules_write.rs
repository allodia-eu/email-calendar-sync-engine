//! A neutral rule written as a Gmail filter (`filters-and-out-of-office.md`, §5), the inverse of
//! `crate::rules_read`.
//!
//! `plan_rules` has already decided what the Gmail vocabulary holds. Conditions go into the
//! criteria Gmail has for them (`from`, `to`, `subject`, `hasAttachment`, `size`), and a second
//! one of a kind into `query` as a search term; any-of conditions are one `query` of terms joined
//! by ` OR `, and the exceptions one `negatedQuery`. A native part puts its own keys back.
//!
//! A few things are finer than the plan's grain, and this writer is where they are found: a value
//! holding a double quote or a line break, which no search term spells; a native part whose key a
//! typed part also writes; a role Gmail keeps as no label to file into (Sent, Drafts), or a copy
//! into a role; an importance neither high nor low; a keyword no label was named for. The writer
//! names the part ([`RulePart`]), never its value, and the caller writes nothing for the rule and
//! keeps the filter it was read as.

use std::collections::BTreeMap;

use engine_core::{
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{Action, Condition, MailRule, MatchMode, NativeKind, RuleBody, RuleMailbox, RulePart},
};
use serde_json::{Map, Value, json};

use crate::rules_query::{Term, render_values};

/// `rule` as a Gmail filter body (`criteria` and `action`), with `label` the id of the label a
/// keyword is kept under.
///
/// # Errors
///
/// The part Gmail cannot hold as the rule says it (module docs).
pub(crate) fn filter(
    rule: &MailRule,
    label: &BTreeMap<Keyword, String>,
) -> Result<Value, RulePart> {
    let RuleBody::Typed(typed) = &rule.body else {
        return Err(RulePart::Rule);
    };
    let mut criteria = Map::new();
    let mut query = Vec::new();
    let when = &typed.when;
    if when.mode == MatchMode::Any && when.conditions.len() > 1 {
        let mut terms = Vec::new();
        for (i, condition) in when.conditions.iter().enumerate() {
            terms.push(term(condition).ok_or(RulePart::Condition(i))?);
        }
        criteria.insert("query".to_owned(), json!(terms.join(" OR ")));
    } else {
        let mut first_term = None;
        for (i, condition) in when.conditions.iter().enumerate() {
            let held = query.len();
            criterion(&mut criteria, &mut query, condition).ok_or(RulePart::Condition(i))?;
            if query.len() > held {
                first_term.get_or_insert(i);
            }
        }
        if let Some(at) = first_term {
            put(&mut criteria, "query", json!(query.join(" "))).ok_or(RulePart::Condition(at))?;
        }
    }
    if !when.except.is_empty() {
        let mut terms = Vec::new();
        for (i, exception) in when.except.iter().enumerate() {
            terms.push(term(exception).ok_or(RulePart::Exception(i))?);
        }
        put(&mut criteria, "negatedQuery", json!(terms.join(" OR ")))
            .ok_or(RulePart::Exception(0))?;
    }
    let mut action = Labels::default();
    for (i, act) in typed.actions.iter().enumerate() {
        action.add(act, label).ok_or(RulePart::Action(i))?;
    }
    Ok(json!({ "criteria": criteria, "action": action.into_value() }))
}

fn term(condition: &Condition) -> Option<String> {
    Term::of(condition)?.render()
}

/// Puts `value` under `key`, where no other part has: two parts writing one key would each be
/// read as the other.
fn put(map: &mut Map<String, Value>, key: &str, value: Value) -> Option<()> {
    if map.contains_key(key) {
        return None;
    }
    map.insert(key.to_owned(), value);
    Some(())
}

/// Adds one condition: into the criterion Gmail has for it where that is free, else as a term
/// of `query`; a native part as its own keys.
fn criterion(
    criteria: &mut Map<String, Value>,
    query: &mut Vec<String>,
    condition: &Condition,
) -> Option<()> {
    if let Condition::Native(native) = condition {
        if native.kind != NativeKind::Gmail {
            return None;
        }
        let Value::Object(fragment) = serde_json::from_str(&native.raw).ok()? else {
            return None;
        };
        for (key, value) in fragment {
            put(criteria, &key, value)?;
        }
        return Some(());
    }
    let term = Term::of(condition)?;
    let field = match &term {
        Term::From(values) => Some(("from", values)),
        Term::To(values) => Some(("to", values)),
        Term::Subject(values) => Some(("subject", values)),
        _ => None,
    };
    match (&term, field) {
        (_, Some((key, values))) if !criteria.contains_key(key) => {
            criteria.insert(key.to_owned(), json!(render_values(values)?));
        }
        (Term::HasAttachment, _) => {
            criteria.insert("hasAttachment".to_owned(), Value::Bool(true));
        }
        (Term::Larger(bytes) | Term::Smaller(bytes), _) if !criteria.contains_key("size") => {
            let comparison = if matches!(term, Term::Larger(_)) {
                "larger"
            } else {
                "smaller"
            };
            criteria.insert("size".to_owned(), json!(bytes));
            criteria.insert("sizeComparison".to_owned(), json!(comparison));
        }
        _ => query.push(term.render()?),
    }
    Some(())
}

/// The label changes and the forward a rule's actions add up to.
#[derive(Default)]
struct Labels {
    add: Vec<String>,
    remove: Vec<String>,
    forward: Option<String>,
    other: Map<String, Value>,
}

impl Labels {
    fn push(list: &mut Vec<String>, id: &str) {
        if !list.iter().any(|held| held == id) {
            list.push(id.to_owned());
        }
    }

    fn add(&mut self, action: &Action, label: &BTreeMap<Keyword, String>) -> Option<()> {
        match action {
            Action::FileInto {
                mailbox: RuleMailbox::Id(id),
                copy,
            } => {
                Self::push(&mut self.add, id.as_str());
                if !copy {
                    Self::push(&mut self.remove, "INBOX");
                }
            }
            Action::FileInto {
                mailbox: RuleMailbox::Role(role),
                copy: false,
            } => match role {
                MailboxRole::Trash => Self::push(&mut self.add, "TRASH"),
                MailboxRole::Junk => Self::push(&mut self.add, "SPAM"),
                MailboxRole::Archive => Self::push(&mut self.remove, "INBOX"),
                _ => return None,
            },
            // Gmail deletes to Trash, where the message can still be recovered.
            Action::Discard => Self::push(&mut self.add, "TRASH"),
            Action::Redirect { to, copy: true } | Action::Forward { to } => match to.as_slice() {
                [one] if self.forward.is_none() => self.forward = Some(one.as_str().to_owned()),
                _ => return None,
            },
            Action::AddKeywords(keywords) if !keywords.is_empty() => {
                for keyword in keywords {
                    match keyword.as_system() {
                        Some(SystemKeyword::Seen) => Self::push(&mut self.remove, "UNREAD"),
                        Some(SystemKeyword::Flagged) => Self::push(&mut self.add, "STARRED"),
                        Some(_) => return None,
                        None => Self::push(&mut self.add, label.get(keyword)?),
                    }
                }
            }
            Action::MarkImportance(Importance::High) => Self::push(&mut self.add, "IMPORTANT"),
            Action::MarkImportance(Importance::Low) => Self::push(&mut self.remove, "IMPORTANT"),
            Action::Native(native) if native.kind == NativeKind::Gmail => {
                let Value::Object(fragment) = serde_json::from_str(&native.raw).ok()? else {
                    return None;
                };
                for (key, value) in fragment {
                    match (key.as_str(), value) {
                        ("addLabelIds", Value::Array(ids)) => {
                            for id in ids.iter().map(Value::as_str) {
                                Self::push(&mut self.add, id?);
                            }
                        }
                        ("removeLabelIds", Value::Array(ids)) => {
                            for id in ids.iter().map(Value::as_str) {
                                Self::push(&mut self.remove, id?);
                            }
                        }
                        ("forward", Value::String(address)) if self.forward.is_none() => {
                            self.forward = Some(address);
                        }
                        (key, value) => put(&mut self.other, key, value)?,
                    }
                }
            }
            _ => return None,
        }
        Some(())
    }

    fn into_value(self) -> Value {
        let mut out = self.other;
        if !self.add.is_empty() {
            out.insert("addLabelIds".to_owned(), json!(self.add));
        }
        if !self.remove.is_empty() {
            out.insert("removeLabelIds".to_owned(), json!(self.remove));
        }
        if let Some(forward) = self.forward {
            out.insert("forward".to_owned(), json!(forward));
        }
        Value::Object(out)
    }
}

#[cfg(test)]
#[path = "rules_write_tests.rs"]
mod tests;
