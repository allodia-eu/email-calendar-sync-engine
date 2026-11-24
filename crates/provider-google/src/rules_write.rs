//! A neutral rule written as a Gmail filter (`filters-and-out-of-office.md`, §5), the inverse of
//! `crate::rules_read`.
//!
//! `plan_rules` has already decided what the Gmail vocabulary holds, limits included (one user
//! label, a phrase for every value, a native condition only among all-of conditions, roles only
//! Trash, Junk and the archive, importance high or low, one forward): the writer is not where
//! those are found. A native part puts its own keys back first. Then each condition goes into the
//! criterion Gmail has for it (`from`, `subject`, `hasAttachment`, `size`) where that is free, and
//! otherwise into `query` as a search term, after a native query held there in brackets;
//! any-of conditions are one `query` of terms joined by ` OR `, and the exceptions one
//! `negatedQuery`, after a native one held there in brackets.
//!
//! What the plan cannot see: two native parts with one key (a host joining provider fragments by
//! hand; a set read from Gmail never holds them), and a native part not as Gmail's JSON. The writer
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
    let when = &typed.when;
    // A native part first: it holds its own keys, and a typed condition goes where it can.
    for (i, condition) in when.conditions.iter().enumerate() {
        if let Condition::Native(native) = condition {
            native_keys(&mut criteria, native.kind, &native.raw).ok_or(RulePart::Condition(i))?;
        }
    }
    if when.mode == MatchMode::Any && when.conditions.len() > 1 {
        let mut terms = Vec::new();
        for (i, condition) in when.conditions.iter().enumerate() {
            terms.push(term(condition).ok_or(RulePart::Condition(i))?);
        }
        criteria.insert("query".to_owned(), json!(terms.join(" OR ")));
    } else {
        let mut query = Vec::new();
        for (i, condition) in when.conditions.iter().enumerate() {
            if !matches!(condition, Condition::Native(_)) {
                criterion(&mut criteria, &mut query, condition).ok_or(RulePart::Condition(i))?;
            }
        }
        if !query.is_empty() {
            let joined = after_native(&criteria, "query", &query, " ").ok_or(RulePart::Mode)?;
            criteria.insert("query".to_owned(), json!(joined));
        }
    }
    if !when.except.is_empty() {
        let mut terms = Vec::new();
        for (i, exception) in when.except.iter().enumerate() {
            terms.push(term(exception).ok_or(RulePart::Exception(i))?);
        }
        let joined = after_native(&criteria, "negatedQuery", &terms, " OR ")
            .ok_or(RulePart::Exception(0))?;
        criteria.insert("negatedQuery".to_owned(), json!(joined));
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

/// `terms` joined by `joint`, after the search a native part holds under `key`, in brackets so
/// that it stays one term: `(native) a b`, or `(native) OR a OR b`.
fn after_native(
    criteria: &Map<String, Value>,
    key: &str,
    terms: &[String],
    joint: &str,
) -> Option<String> {
    let joined = terms.join(joint);
    match criteria.get(key) {
        None => Some(joined),
        Some(Value::String(native)) => Some(format!("({native}){joint}{joined}")),
        Some(_) => None,
    }
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

/// A native part's own keys, put back as they were read.
fn native_keys(map: &mut Map<String, Value>, kind: NativeKind, raw: &str) -> Option<()> {
    if kind != NativeKind::Gmail {
        return None;
    }
    let Value::Object(fragment) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    for (key, value) in fragment {
        put(map, &key, value)?;
    }
    Some(())
}

/// Adds one typed condition: into the criterion Gmail has for it where that is free, else as a
/// term of `query`.
fn criterion(
    criteria: &mut Map<String, Value>,
    query: &mut Vec<String>,
    condition: &Condition,
) -> Option<()> {
    let term = Term::of(condition)?;
    let field = match &term {
        Term::From(values) => Some(("from", values)),
        Term::Subject(values) => Some(("subject", values)),
        _ => None,
    };
    match (&term, field) {
        (_, Some((key, values))) if !criteria.contains_key(key) => {
            criteria.insert(key.to_owned(), json!(render_values(values)?));
        }
        (Term::HasAttachment, _) if criteria.get("hasAttachment") == Some(&Value::Bool(true)) => {}
        (Term::HasAttachment, _) if !criteria.contains_key("hasAttachment") => {
            criteria.insert("hasAttachment".to_owned(), Value::Bool(true));
        }
        (Term::Larger(bytes) | Term::Smaller(bytes), _)
            if !criteria.contains_key("size") && !criteria.contains_key("sizeComparison") =>
        {
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
                [one] => self.forward(one.as_str())?,
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
                        ("forward", Value::String(address)) => self.forward(&address)?,
                        // A forward of another shape is refused, never kept as some other key.
                        ("forward", _) => return None,
                        (key, value) => put(&mut self.other, key, value)?,
                    }
                }
            }
            _ => return None,
        }
        Some(())
    }

    /// Sends the message on to `address`: one forward per filter, so a second is refused rather
    /// than either dropped.
    fn forward(&mut self, address: &str) -> Option<()> {
        if self.forward.is_some() {
            return None;
        }
        self.forward = Some(address.to_owned());
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
