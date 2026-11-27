//! A filter's folder on Gmail, end to end offline: named from the account's labels as the adapter
//! lists them (`tests/fixtures/mail/labels.json`, Gmail's own), written by the adapter's writer,
//! read back by its reader, and resolved to the label it was named from.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    mail::{Mailbox, MailboxRole},
    rules::{Action, Condition, HeaderField, MailRule, Match, RuleBody, RuleMailbox},
};
use serde_json::Value;

use super::*;
use crate::{
    named_labels::NamedLabels,
    normalize::{all_mail_mailbox, label_from_json, nest_labels},
    rules_read, rules_write,
};

/// The account's labels as the adapter lists them, All Mail included.
fn folders() -> Vec<Mailbox> {
    let dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    let text = std::fs::read_to_string(format!("{dir}/tests/fixtures/mail/labels.json")).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    let mut folders: Vec<Mailbox> = doc["labels"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|label| label_from_json(label).unwrap())
        .collect();
    nest_labels(&mut folders);
    folders.push(all_mail_mailbox());
    folders
}

/// The filter's folder as the adapter reads back the filter its writer wrote for a rule filing
/// into `mailbox`.
fn round_trip(mailbox: RuleMailbox) -> RuleMailbox {
    let subject = Condition::Header {
        field: HeaderField::Subject,
        test: crate::rules_read::tests::words(&["invoice"]),
    };
    let rule = MailRule::typed(
        Match::all(vec![subject]),
        vec![Action::FileInto {
            mailbox,
            copy: false,
        }],
    );
    let filter = rules_write::filter(&rule, &BTreeMap::new()).expect("written");
    let read = rules_read::rule(&filter, 0, 0, &NamedLabels::default());
    let RuleBody::Typed(typed) = &read.body else {
        panic!("read typed");
    };
    let [Action::FileInto { mailbox, .. }] = typed.actions.as_slice() else {
        panic!("one filing read back: {:?}", typed.actions);
    };
    mailbox.clone()
}

#[test]
fn a_user_label_is_filed_by_id_trash_and_spam_by_role_and_nothing_else() {
    let vocabulary = vocabulary(&BTreeSet::new());
    let folders = folders();
    let mut named = Vec::new();
    for folder in &folders {
        let Some(mailbox) = RuleMailbox::for_folder(folder, &folders, &vocabulary) else {
            continue;
        };
        let read = round_trip(mailbox.clone());
        assert_eq!(
            read.folder_in(&folders, &vocabulary).map(|f| &f.id),
            Some(&folder.id),
            "{mailbox:?} read back as {read:?}"
        );
        named.push((folder.id.as_str().to_owned(), mailbox));
    }
    let id = |id: &str| RuleMailbox::Id(engine_core::ids::MailboxId::try_from(id).unwrap());
    assert_eq!(
        named,
        [
            ("TRASH".to_owned(), RuleMailbox::Role(MailboxRole::Trash)),
            ("SPAM".to_owned(), RuleMailbox::Role(MailboxRole::Junk)),
            ("Label_1".to_owned(), id("Label_1")),
            ("Label_2".to_owned(), id("Label_2")),
            ("Label_3".to_owned(), id("Label_3")),
            ("Label_4".to_owned(), id("Label_4")),
        ],
        "the inbox, Sent, Drafts, Important and All Mail are no label a filter adds"
    );
}
