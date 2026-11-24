//! A neutral rule written as a Graph `messageRule` body (`filters-and-out-of-office.md`, §5).
//!
//! `plan_rules` has already decided what the Graph vocabulary holds, at its grain. A few things
//! are finer than that grain, and this writer is where they are found:
//!
//! - **One predicate per kind.** Graph's predicates are keys of one object, and the values of one
//!   key are any-of. Two conditions that write the same key (two subject tests, a domain and a
//!   sender test) would be read back as either one, which widens the rule: refused. In the
//!   exceptions, where any one already vetoes, they merge.
//! - **Which addresses a test can name.** Graph tests the sender, or the `To` and `Cc` recipients
//!   together: a test of `To` alone or `Cc` alone, or of a domain by anything but equality, would
//!   widen the rule, and is refused.
//! - **One folder per kind of filing**, and a role Graph has a well-known folder for.
//!
//! A refusal names the part ([`RulePart`]), never its value. The caller reports the rule as not
//! kept: dropping a condition would widen it, and an action is refused only where one of its kind
//! is already there.

use engine_core::{
    ids::MailboxId,
    mail::{Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, MatchMode, MeField,
        NativeKind, RuleAddress, RuleBody, RuleMailbox, RulePart, TextOp, TextTest,
    },
};
use serde_json::{Map, Value, json};

/// The well-known folder name Graph resolves a role to, for the roles a rule can file into.
pub(crate) fn well_known(role: &MailboxRole) -> Option<&'static str> {
    Some(match role {
        MailboxRole::Inbox => "inbox",
        MailboxRole::Archive => "archive",
        MailboxRole::Drafts => "drafts",
        MailboxRole::Sent => "sentitems",
        MailboxRole::Trash => "deleteditems",
        MailboxRole::Junk => "junkemail",
        _ => return None,
    })
}

/// The roles `rule` files into by way of a folder id: every one but a move to Trash, which is
/// Graph's own `delete`.
pub(crate) fn roles_needed(rule: &MailRule) -> Vec<MailboxRole> {
    let RuleBody::Typed(typed) = &rule.body else {
        return Vec::new();
    };
    typed
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Trash),
                copy: false,
            } => None,
            Action::FileInto {
                mailbox: RuleMailbox::Role(filed),
                ..
            } => Some(filed.clone()),
            _ => None,
        })
        .collect()
}

/// `rule` as a Graph `messageRule` body, without its `sequence`; `folder` answers the id of a
/// role's folder ([`roles_needed`]).
///
/// # Errors
///
/// The part Graph cannot hold as the rule says it (module docs).
pub(crate) fn body(
    rule: &MailRule,
    folder: &dyn Fn(&MailboxRole) -> Option<MailboxId>,
) -> Result<Map<String, Value>, RulePart> {
    let RuleBody::Typed(typed) = &rule.body else {
        return Err(RulePart::Rule);
    };
    if typed.when.mode == MatchMode::Any && typed.when.conditions.len() > 1 {
        return Err(RulePart::Mode);
    }
    let mut conditions = Map::new();
    for (i, condition) in typed.when.conditions.iter().enumerate() {
        predicate(&mut conditions, condition, false).ok_or(RulePart::Condition(i))?;
    }
    let mut exceptions = Map::new();
    for (i, exception) in typed.when.except.iter().enumerate() {
        predicate(&mut exceptions, exception, true).ok_or(RulePart::Exception(i))?;
    }
    let mut actions = Map::new();
    for (i, action) in typed.actions.iter().enumerate() {
        act(&mut actions, action, folder).ok_or(RulePart::Action(i))?;
    }
    if typed.stop {
        actions.insert("stopProcessingRules".to_owned(), Value::Bool(true));
    }
    let mut out = Map::new();
    if let Some(name) = &rule.name {
        out.insert("displayName".to_owned(), json!(name.as_str()));
    }
    out.insert("isEnabled".to_owned(), Value::Bool(rule.enabled));
    out.insert("conditions".to_owned(), Value::Object(conditions));
    out.insert("exceptions".to_owned(), Value::Object(exceptions));
    out.insert("actions".to_owned(), Value::Object(actions));
    Ok(out)
}

/// Clears in `new` every predicate and action `old` holds that `new` does not, so a `PATCH`
/// leaves none of the old rule's parts behind whether Graph merges the objects or replaces them:
/// a flag as `false`, anything else as `null`.
pub(crate) fn clear_dropped(new: &mut Map<String, Value>, old: &Value) {
    for section in ["conditions", "exceptions", "actions"] {
        let Some(old) = old.get(section).and_then(Value::as_object) else {
            continue;
        };
        let Some(Value::Object(new)) = new.get_mut(section) else {
            continue;
        };
        for (key, value) in old {
            if !new.contains_key(key) {
                let cleared = if value.is_boolean() {
                    Value::Bool(false)
                } else {
                    Value::Null
                };
                new.insert(key.clone(), cleared);
            }
        }
    }
}

/// Adds `value` under `key`: a list merges where any-of is meant (`merge`), and otherwise a key
/// already there must already say the same.
fn put(map: &mut Map<String, Value>, key: &str, value: Value, merge: bool) -> Option<()> {
    match map.get_mut(key) {
        None => {
            map.insert(key.to_owned(), value);
            Some(())
        }
        Some(Value::Array(held)) if merge => {
            held.extend(value.as_array()?.iter().cloned());
            Some(())
        }
        Some(held) => (*held == value).then_some(()),
    }
}

fn values(test: &TextTest) -> Value {
    json!(test.values())
}

fn recipients(addresses: impl IntoIterator<Item = impl AsRef<str>>) -> Value {
    Value::Array(
        addresses
            .into_iter()
            .map(|address| json!({ "emailAddress": { "address": address.as_ref() } }))
            .collect(),
    )
}

/// Adds `condition` to a predicates object: the conditions (all hold) or, with `except`, the
/// exceptions (any one vetoes). `None` where it cannot be put there as it says.
fn predicate(map: &mut Map<String, Value>, condition: &Condition, except: bool) -> Option<()> {
    let flag = |map: &mut Map<String, Value>, key: &str| put(map, key, Value::Bool(true), false);
    match condition {
        Condition::Header {
            field: HeaderField::Subject,
            test,
        } if test.op() == TextOp::Contains => put(map, "subjectContains", values(test), except),
        Condition::Body(test) if test.op() == TextOp::Contains => {
            put(map, "bodyContains", values(test), except)
        }
        Condition::Address { field, part, test } => {
            let (key, value) = address(*field, *part, test)?;
            put(map, key, value, except)
        }
        Condition::SentToMe(field) => flag(
            map,
            match field {
                MeField::To => "sentToMe",
                MeField::Cc => "sentCcMe",
                MeField::ToOrCc => "sentToOrCcMe",
                MeField::OnlyTo => "sentOnlyToMe",
                MeField::NotTo => "notSentToMe",
                _ => return None,
            },
        ),
        Condition::HasAttachment => flag(map, "hasAttachments"),
        Condition::Importance(importance) => {
            put(map, "importance", json!(importance.as_str()), false)
        }
        Condition::Native(native) if native.kind == NativeKind::Graph => {
            merge_native(map, &native.raw, except)
        }
        _ => None,
    }
}

/// The predicate an address test is, where Graph has one that means the same.
fn address(
    field: AddressField,
    part: AddressPart,
    test: &TextTest,
) -> Option<(&'static str, Value)> {
    let (exact, contains) = match field {
        AddressField::From => ("fromAddresses", "senderContains"),
        AddressField::ToOrCc => ("sentToAddresses", "recipientContains"),
        _ => return None,
    };
    match (part, test.op()) {
        (AddressPart::All, TextOp::Is) => Some((exact, recipients(test.values()))),
        (AddressPart::All, TextOp::Contains) => Some((contains, values(test))),
        // A domain is the text after the `@`: `@example.com` within the address.
        (AddressPart::Domain, TextOp::Is) => Some((
            contains,
            json!(
                test.values()
                    .iter()
                    .map(|domain| format!("@{domain}"))
                    .collect::<Vec<_>>()
            ),
        )),
        _ => None,
    }
}

/// Merges a native fragment (one object, as read) into `map`.
fn merge_native(map: &mut Map<String, Value>, raw: &str, merge: bool) -> Option<()> {
    let Value::Object(fragment) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    for (key, value) in fragment {
        put(map, &key, value, merge)?;
    }
    Some(())
}

fn act(
    map: &mut Map<String, Value>,
    action: &Action,
    folder: &dyn Fn(&MailboxRole) -> Option<MailboxId>,
) -> Option<()> {
    match action {
        Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Trash),
            copy: false,
        } => put(map, "delete", Value::Bool(true), false),
        Action::FileInto { mailbox, copy } => {
            let id = match mailbox {
                RuleMailbox::Id(id) => id.clone(),
                RuleMailbox::Role(role) => folder(role)?,
                _ => return None,
            };
            let key = if *copy {
                "copyToFolder"
            } else {
                "moveToFolder"
            };
            put(map, key, json!(id.as_str()), false)
        }
        Action::Discard => put(map, "permanentDelete", Value::Bool(true), false),
        Action::Redirect { to, copy: true } if !to.is_empty() => put(
            map,
            "redirectTo",
            recipients(to.iter().map(RuleAddress::as_str)),
            true,
        ),
        Action::Forward { to } if !to.is_empty() => put(
            map,
            "forwardTo",
            recipients(to.iter().map(RuleAddress::as_str)),
            true,
        ),
        Action::AddKeywords(keywords) if !keywords.is_empty() => {
            for keyword in keywords {
                keyword_action(map, keyword)?;
            }
            Some(())
        }
        Action::MarkImportance(importance) => {
            put(map, "markImportance", json!(importance.as_str()), false)
        }
        Action::Native(native) if native.kind == NativeKind::Graph => {
            merge_native(map, &native.raw, true)
        }
        _ => None,
    }
}

/// `$seen` is `markAsRead`; any keyword but a system one is a category of its name.
fn keyword_action(map: &mut Map<String, Value>, keyword: &Keyword) -> Option<()> {
    match keyword.as_system() {
        Some(SystemKeyword::Seen) => put(map, "markAsRead", Value::Bool(true), false),
        Some(_) => None,
        None => put(map, "assignCategories", json!([keyword.as_str()]), true),
    }
}

#[cfg(test)]
#[path = "rules_write_tests.rs"]
mod tests;
