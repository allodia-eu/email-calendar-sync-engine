//! A neutral rule written as a Graph `messageRule` body (`filters-and-out-of-office.md`, §5).
//!
//! `plan_rules` has already decided what the Graph vocabulary holds, at its grain. A few things
//! are finer than that grain, and this writer is where they are found:
//!
//! - **One predicate per kind.** Graph's predicates are keys of one object, and the values of one
//!   key are any-of. Two conditions that write the same key (two subject tests) would be read back
//!   as either one, which widens the rule: refused. In the exceptions, where any one already
//!   vetoes, they merge.
//! - **Which addresses a test can name.** Graph tests the sender, or the `To` and `Cc` recipients
//!   together, by equality or containment, never a domain alone.
//! - **One folder per kind of filing**, and a role Graph has a well-known folder for.
//!
//! The Graph vocabulary states each of these (`crate::rules::vocabulary`, its `RuleLimit`s), so
//! `plan_rules` has already left out what this writer cannot place: a refusal here is a bug. It
//! names the part ([`RulePart`]), never its value, and the caller keeps the server's rule as it
//! was and reports a failure.

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

/// Gives each recipient in `new` the display name `old` had for the same address, under the same
/// key: the model tests addresses alone, and a `PATCH` should not drop the names a user sees.
pub(crate) fn keep_names(new: &mut Map<String, Value>, old: &Value) {
    for section in ["conditions", "exceptions", "actions"] {
        let Some(Value::Object(new)) = new.get_mut(section) else {
            continue;
        };
        for (key, value) in new.iter_mut() {
            let Some(entries) = value.as_array_mut() else {
                continue;
            };
            let named = old[section][key.as_str()].as_array();
            for entry in entries {
                let address = entry["emailAddress"]["address"].clone();
                let name = named
                    .into_iter()
                    .flatten()
                    .find(|was| was["emailAddress"]["address"] == address)
                    .and_then(|was| was["emailAddress"].get("name"))
                    .cloned();
                if let (Some(name), Some(Value::Object(email))) =
                    (name, entry.get_mut("emailAddress"))
                {
                    email.insert("name".to_owned(), name);
                }
            }
        }
    }
}

/// Whether `new` drops a part `old` holds: a predicate or an action it asks for, or its name.
/// Graph keeps such a part on a `PATCH` whatever is sent to clear it (`null` was observed to clear
/// nothing), so a rule that drops one is written anew.
pub(crate) fn drops(new: &Map<String, Value>, old: &Value) -> bool {
    let asks = |value: &Value| match value {
        Value::Null | Value::Bool(false) => false,
        Value::Array(items) => !items.is_empty(),
        _ => true,
    };
    let named =
        |value: Option<&Value>| value.and_then(Value::as_str).is_some_and(|n| !n.is_empty());
    if named(old.get("displayName")) && !named(new.get("displayName")) {
        return true;
    }
    ["conditions", "exceptions", "actions"]
        .iter()
        .any(|section| {
            let kept = new.get(*section).and_then(Value::as_object);
            old.get(*section)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .any(|(key, value)| asks(value) && !kept.is_some_and(|kept| kept.contains_key(key)))
        })
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
        // A domain as `@domain` within the address would also match `@domain.evil.test`.
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
