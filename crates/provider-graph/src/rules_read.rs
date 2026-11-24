//! Graph `messageRule`s read into the neutral model (`filters-and-out-of-office.md`, §5).
//!
//! A rule's `conditions` and `exceptions` are each one `messageRulePredicates` object: every
//! predicate in it must hold (conditions), or any one vetoes the rule (exceptions), and the values
//! of one predicate are any-of. A predicate the model types becomes a typed [`Condition`]; every
//! other one (an Exchange message class, `headerContains`, `withinSizeRange`, …) becomes one
//! [`NativeCondition`] holding that predicate alone, so nothing is dropped and a write to Graph
//! puts it back as it was. Actions go the same way.
//!
//! Reading never fails on a rule: what does not fit a typed part is carried native. A predicate
//! set to `false` (`hasAttachments: false`) asks for nothing and is left out, as Graph leaves it
//! out.
//!
//! The rule's `id` stays in the listing kept as the set's raw form; the rule's origin is its index
//! there ([`RuleOrigin::Listed`]).

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, MeField,
        NativeAction, NativeCondition, NativeKind, RuleAddress, RuleMailbox, RuleName, RuleOrigin,
        TextOp, TextTest,
    },
};
use serde_json::{Map, Value};

/// The rule at `index` of the listing whose fingerprint is `set`.
pub(crate) fn rule(value: &Value, set: u64, index: usize) -> MailRule {
    let conditions = predicates(value.get("conditions"));
    let except = predicates(value.get("exceptions"));
    let (actions, stop) = actions(value.get("actions"));
    let mut rule = MailRule::typed(Match::all(conditions).except(except), actions)
        .with_origin(RuleOrigin::Listed { set, index });
    if stop {
        rule = rule.stopping();
    }
    if let Some(name) = value
        .get("displayName")
        .and_then(Value::as_str)
        .and_then(|name| RuleName::new(name).ok())
    {
        rule = rule.named(name);
    }
    if value.get("isEnabled").and_then(Value::as_bool) == Some(false) {
        rule = rule.disabled();
    }
    if flag(value, "hasError") {
        rule = rule.marked_in_error();
    }
    if flag(value, "isReadOnly") {
        rule = rule.read_only();
    }
    rule
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// Whether a predicate or action of this value asks for nothing: `false`, `null`, or an empty
/// list.
fn asks_nothing(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// The value of one key, as a fragment of its own: what a native part carries.
fn fragment(key: &str, value: &Value) -> String {
    let mut one = Map::new();
    one.insert(key.to_owned(), value.clone());
    Value::Object(one).to_string()
}

fn predicates(object: Option<&Value>) -> Vec<Condition> {
    let Some(object) = object.and_then(Value::as_object) else {
        return Vec::new();
    };
    object
        .iter()
        .filter(|(_, value)| !asks_nothing(value))
        .map(|(key, value)| {
            typed_condition(key, value).unwrap_or_else(|| {
                Condition::Native(NativeCondition::new(
                    NativeKind::Graph,
                    fragment(key, value),
                ))
            })
        })
        .collect()
}

/// The typed condition one predicate is, or `None` where the model has none for it.
fn typed_condition(key: &str, value: &Value) -> Option<Condition> {
    let address = |field, op, values| {
        Some(Condition::Address {
            field,
            part: AddressPart::All,
            test: TextTest::new(op, values).ok()?,
        })
    };
    let me = |field| (value == &Value::Bool(true)).then_some(Condition::SentToMe(field));
    match key {
        "subjectContains" => Some(Condition::Header {
            field: HeaderField::Subject,
            test: contains(value)?,
        }),
        "bodyContains" => Some(Condition::Body(contains(value)?)),
        "fromAddresses" => address(AddressField::From, TextOp::Is, recipients(value)?),
        "senderContains" => address(AddressField::From, TextOp::Contains, strings(value)?),
        "sentToAddresses" => address(AddressField::ToOrCc, TextOp::Is, recipients(value)?),
        "recipientContains" => address(AddressField::ToOrCc, TextOp::Contains, strings(value)?),
        "hasAttachments" => (value == &Value::Bool(true)).then_some(Condition::HasAttachment),
        "importance" => value
            .as_str()
            .and_then(Importance::parse)
            .map(Condition::Importance),
        "sentToMe" => me(MeField::To),
        "sentCcMe" => me(MeField::Cc),
        "sentToOrCcMe" => me(MeField::ToOrCc),
        "sentOnlyToMe" => me(MeField::OnlyTo),
        "notSentToMe" => me(MeField::NotTo),
        _ => None,
    }
}

fn contains(value: &Value) -> Option<TextTest> {
    TextTest::new(TextOp::Contains, strings(value)?).ok()
}

/// A list of strings, or `None` where any item is not one.
fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// The addresses of a list of Graph `recipient`s, or `None` where any has none. The display name
/// beside each is not a part of the test.
fn recipients(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|recipient| {
            recipient
                .get("emailAddress")?
                .get("address")?
                .as_str()
                .filter(|address| !address.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn actions(object: Option<&Value>) -> (Vec<Action>, bool) {
    let Some(object) = object.and_then(Value::as_object) else {
        return (Vec::new(), false);
    };
    let mut stop = false;
    let mut out = Vec::new();
    for (key, value) in object.iter().filter(|(_, value)| !asks_nothing(value)) {
        if key == "stopProcessingRules" {
            stop |= value == &Value::Bool(true);
            continue;
        }
        out.push(typed_action(key, value).unwrap_or_else(|| native_action(key, value)));
    }
    (out, stop)
}

/// A native action: one that may send mail unless it is one known not to.
fn native_action(key: &str, value: &Value) -> Action {
    let native = NativeAction::new(NativeKind::Graph, fragment(key, value));
    Action::Native(if key == "assignCategories" {
        native.sending_no_mail()
    } else {
        native
    })
}

/// The typed action one Graph action is, or `None` where the model has none for it.
fn typed_action(key: &str, value: &Value) -> Option<Action> {
    let folder = |copy| {
        let id = MailboxId::try_from(value.as_str()?).ok()?;
        Some(Action::FileInto {
            mailbox: RuleMailbox::Id(id),
            copy,
        })
    };
    let set = |yes: Action| (value == &Value::Bool(true)).then_some(yes);
    match key {
        "moveToFolder" => folder(false),
        "copyToFolder" => folder(true),
        // Exchange's `delete` moves the message to Deleted Items.
        "delete" => set(Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Trash),
            copy: false,
        }),
        "permanentDelete" => set(Action::Discard),
        // Exchange redirects keeping the message in the mailbox too.
        "redirectTo" => Some(Action::Redirect {
            to: addresses(value)?,
            copy: true,
        }),
        "forwardTo" => Some(Action::Forward {
            to: addresses(value)?,
        }),
        "markAsRead" => set(Action::AddKeywords(BTreeSet::from([Keyword::system(
            SystemKeyword::Seen,
        )]))),
        "markImportance" => value
            .as_str()
            .and_then(Importance::parse)
            .map(Action::MarkImportance),
        "assignCategories" => categories(value).map(Action::AddKeywords),
        _ => None,
    }
}

fn addresses(value: &Value) -> Option<Vec<RuleAddress>> {
    recipients(value)?
        .into_iter()
        .map(|address| RuleAddress::new(address).ok())
        .collect()
}

/// Categories as named keywords, where every one is a keyword as it stands: a category's name
/// keeps its case and may hold a space, which a keyword cannot, and such a list stays native.
fn categories(value: &Value) -> Option<BTreeSet<Keyword>> {
    strings(value)?
        .into_iter()
        .map(|name| {
            Keyword::new(name.as_str())
                .ok()
                .filter(|keyword| keyword.as_str() == name && keyword.as_system().is_none())
        })
        .collect()
}

#[cfg(test)]
#[path = "rules_read_tests.rs"]
pub(crate) mod tests;
