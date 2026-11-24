//! Gmail filters read into the neutral model (`filters-and-out-of-office.md`, §3 and §5).
//!
//! A filter is `{ id, criteria, action }`. Every criterion must hold, so a rule's conditions are
//! all-of, except a `query` written wholly as terms joined by ` OR ` beside no other criterion,
//! which is an any-of rule. A `negatedQuery` vetoes the filter where it matches, so where it is
//! terms joined by ` OR ` they are the rule's exceptions. What the search subset does not read
//! (`crate::rules_query`) stays whole, as one native condition per criterion; so does `to`, which
//! matches every recipient (`Bcc` too) as no typed condition does, and `excludeChats`, which
//! narrows what a filter matches and has no neutral counterpart.
//!
//! The actions are label changes and a forward:
//!
//! - **A user's label added** files the message there: moved where the filter also removes `INBOX`,
//!   copied where it does not. `TRASH` and `SPAM` added move it to Trash and Junk. `INBOX` removed
//!   with nothing else to file into archives it (the Archive role).
//! - **A label standing for a keyword** (`crate::named_labels`) adds that keyword; `STARRED` added
//!   flags it, and `UNREAD` removed marks it read.
//! - **`IMPORTANT`** added marks it important (high), removed marks it not (low): Gmail's own
//!   marker, which the vocabulary calls approximate.
//! - **`forward`** sends it on, keeping it, as a redirect would (the sender is kept).
//! - **Any other label change** (an inbox category, `SPAM` removed for "never send it to spam") is
//!   a native action of its own, which sends no mail. So is an action key Gmail may add later: only
//!   `forward` sends, and a Gmail action carries to no other target.
//!
//! Reading never fails on a filter: what does not fit a typed part is carried native. A filter has
//! no name, no order, no stop, and cannot be stored off, so every rule read runs. Its id stays in
//! the listing kept as the set's raw form; the rule's origin is its index there.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, Condition, MailRule, Match, NativeAction, NativeCondition, NativeKind, RuleAddress,
        RuleMailbox, RuleOrigin,
    },
};
use serde_json::{Map, Value};

use crate::{
    named_labels::NamedLabels,
    rules_query::{self, Term},
};

/// How a criterion's values make a term.
type MakeTerm = fn(Vec<String>) -> Term;

/// The criteria whose values are a search the subset may read, and the term each is.
const FIELDS: [(&str, MakeTerm); 2] = [("from", Term::From), ("subject", Term::Subject)];

/// The rule the filter `value` at `index` of the listing whose fingerprint is `set` is.
pub(crate) fn rule(value: &Value, set: u64, index: usize, named: &NamedLabels) -> MailRule {
    let empty = Map::new();
    let criteria = value
        .get("criteria")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let action = value
        .get("action")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    MailRule::typed(when(criteria), actions(action, named))
        .with_origin(RuleOrigin::Listed { set, index })
}

/// A criterion's or an action's value that asks for nothing: `false`, `null`, an empty string or
/// list.
fn asks_nothing(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Keys as one JSON fragment of their own: what a native part carries.
fn fragment<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a Value)>) -> String {
    let one: Map<String, Value> = pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect();
    Value::Object(one).to_string()
}

fn native(pairs: &[(&str, &Value)]) -> Condition {
    Condition::Native(NativeCondition::new(
        NativeKind::Gmail,
        fragment(pairs.iter().copied()),
    ))
}

fn when(criteria: &Map<String, Value>) -> Match {
    let asks = |key: &str| criteria.get(key).filter(|value| !asks_nothing(value));
    let mut conditions = Vec::new();
    for (key, term) in FIELDS {
        if let Some(value) = asks(key) {
            let typed = value
                .as_str()
                .and_then(rules_query::field_values)
                .and_then(|values| term(values).condition());
            conditions.push(typed.unwrap_or_else(|| native(&[(key, value)])));
        }
    }
    if let Some(value) = asks("hasAttachment") {
        conditions.push(if *value == Value::Bool(true) {
            Condition::HasAttachment
        } else {
            native(&[("hasAttachment", value)])
        });
    }
    let size = asks("size").or_else(|| asks("sizeComparison"));
    if size.is_some() {
        conditions.push(size_condition(criteria));
    }
    let query = asks("query");
    let negated = asks("negatedQuery");
    let except = negated
        .and_then(Value::as_str)
        .and_then(rules_query::disjunction)
        .and_then(|terms| {
            terms
                .into_iter()
                .map(Term::condition)
                .collect::<Option<Vec<_>>>()
        });
    if let Some(negated) = negated
        && except.is_none()
    {
        conditions.push(native(&[("negatedQuery", negated)]));
    }
    for (key, value) in criteria {
        let known = matches!(
            key.as_str(),
            "from"
                | "subject"
                | "hasAttachment"
                | "size"
                | "sizeComparison"
                | "query"
                | "negatedQuery"
        );
        if !known && !asks_nothing(value) {
            conditions.push(native(&[(key, value)]));
        }
    }
    let typed = |terms: Vec<Term>| {
        terms
            .into_iter()
            .map(Term::condition)
            .collect::<Option<Vec<_>>>()
    };
    let when = match query {
        None => Match::all(conditions),
        Some(query) => {
            let text = query.as_str();
            let any = text
                .and_then(rules_query::disjunction)
                .filter(|terms| terms.len() > 1)
                .and_then(typed);
            match any {
                // An any-of query holds alone: beside another criterion it would be one of them.
                Some(any) if conditions.is_empty() => Match::any(any),
                _ => {
                    match text.and_then(rules_query::conjunction).and_then(typed) {
                        Some(all) => conditions.extend(all),
                        None => conditions.push(native(&[("query", query)])),
                    }
                    Match::all(conditions)
                }
            }
        }
    };
    when.except(except.unwrap_or_default())
}

/// The size criterion: over or under a number of bytes, or native where it is not as Gmail
/// documents it.
fn size_condition(criteria: &Map<String, Value>) -> Condition {
    let size = criteria.get("size");
    let comparison = criteria.get("sizeComparison");
    let bytes = size.and_then(|size| {
        size.as_u64()
            .or_else(|| size.as_str().and_then(|text| text.parse().ok()))
    });
    let typed = match (bytes, comparison.and_then(Value::as_str)) {
        (Some(bytes), Some("larger")) => Term::Larger(bytes).condition(),
        (Some(bytes), Some("smaller")) => Term::Smaller(bytes).condition(),
        _ => None,
    };
    typed.unwrap_or_else(|| {
        let pairs: Vec<(&str, &Value)> = [("size", size), ("sizeComparison", comparison)]
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        native(&pairs)
    })
}

fn label_ids<'a>(action: &'a Map<String, Value>, key: &str) -> Vec<&'a str> {
    action
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Whether `id` names a label of the user's own (`Label_<n>`), not one of Gmail's.
pub(crate) fn is_user_label(id: &str) -> bool {
    id.strip_prefix("Label_")
        .is_some_and(|rest| !rest.is_empty())
}

fn actions(action: &Map<String, Value>, named: &NamedLabels) -> Vec<Action> {
    let added = label_ids(action, "addLabelIds");
    let removed = label_ids(action, "removeLabelIds");
    let mut used: BTreeSet<(bool, &str)> = BTreeSet::new();
    let archive = removed.contains(&"INBOX");
    let mut out = Vec::new();
    let mut keywords = BTreeSet::new();
    for id in &added {
        if let Some(keyword) = named.keyword(id) {
            keywords.insert(keyword.clone());
            used.insert((true, id));
        } else if let Some(mailbox) = is_user_label(id)
            .then(|| MailboxId::try_from(*id).ok())
            .flatten()
        {
            out.push(Action::FileInto {
                mailbox: RuleMailbox::Id(mailbox),
                copy: !archive,
            });
            used.insert((true, id));
        }
    }
    for (id, role) in [("TRASH", MailboxRole::Trash), ("SPAM", MailboxRole::Junk)] {
        if added.contains(&id) {
            out.push(Action::FileInto {
                mailbox: RuleMailbox::Role(role),
                copy: false,
            });
            used.insert((true, id));
        }
    }
    if archive {
        if out.is_empty() {
            out.push(Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Archive),
                copy: false,
            });
        }
        used.insert((false, "INBOX"));
    }
    for (add, id, keyword) in [
        (true, "STARRED", SystemKeyword::Flagged),
        (false, "UNREAD", SystemKeyword::Seen),
    ] {
        let list = if add { &added } else { &removed };
        if list.contains(&id) {
            keywords.insert(Keyword::system(keyword));
            used.insert((add, id));
        }
    }
    if !keywords.is_empty() {
        out.push(Action::AddKeywords(keywords));
    }
    let important = (added.contains(&"IMPORTANT"), removed.contains(&"IMPORTANT"));
    if important.0 != important.1 {
        let (add, importance) = if important.0 {
            (true, Importance::High)
        } else {
            (false, Importance::Low)
        };
        out.push(Action::MarkImportance(importance));
        used.insert((add, "IMPORTANT"));
    }
    if let Some(forward) = action.get("forward").filter(|value| !asks_nothing(value)) {
        out.push(
            forward
                .as_str()
                .and_then(|address| RuleAddress::new(address).ok())
                .map_or_else(
                    || {
                        Action::Native(NativeAction::new(
                            NativeKind::Gmail,
                            fragment([("forward", forward)]),
                        ))
                    },
                    |address| Action::Redirect {
                        to: vec![address],
                        copy: true,
                    },
                ),
        );
    }
    // Every other label change, one native action each: it sends no mail.
    for (add, key, ids) in [
        (true, "addLabelIds", &added),
        (false, "removeLabelIds", &removed),
    ] {
        for id in ids.iter().filter(|id| !used.contains(&(add, **id))) {
            let list = Value::Array(vec![Value::String((*id).to_owned())]);
            out.push(Action::Native(
                NativeAction::new(NativeKind::Gmail, fragment([(key, &list)])).sending_no_mail(),
            ));
        }
    }
    for (key, value) in action {
        let known = matches!(key.as_str(), "addLabelIds" | "removeLabelIds" | "forward");
        if !known && !asks_nothing(value) {
            // Gmail documents three action keys, and `forward` is the one that sends. A key it adds
            // later is read as sending no mail: a Gmail action carries to Gmail alone, where only
            // a forward is checked and counted.
            out.push(Action::Native(
                NativeAction::new(NativeKind::Gmail, fragment([(key.as_str(), value)]))
                    .sending_no_mail(),
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "rules_read_tests.rs"]
pub(crate) mod tests;
