//! A rule's folder over JMAP Sieve, end to end offline: named from the account's mailboxes as the
//! adapter normalizes them (`tests/fixtures/mailbox_get.json`, Stalwart's), written by the
//! adapter's writer, read back by its reader, and resolved to the mailbox it was named from.

use engine_core::{
    mail::{Mailbox, MailboxRole},
    rules::{Action, MailRule, Match, RuleBody, RuleMailbox},
};
use serde_json::{Value, json};

use super::*;
use crate::{
    mail::mailbox_from_json,
    provider::provider_test_support::{FakeExecutor, fixture},
};

/// The vocabulary of an account whose server lists `extensions`.
fn vocabulary_listing(extensions: &[&str]) -> RuleVocabulary {
    let sieve = json!({ "sieveExtensions": extensions, "maxNumberRedirects": 1 });
    let session = json!({
        "capabilities": { capability::CORE: {}, capability::MAIL: {}, capability::SIEVE: {} },
        "primaryAccounts": { capability::MAIL: "c", capability::SIEVE: "c" },
        "accounts": { "c": { "accountCapabilities": {
            capability::MAIL: {}, capability::SIEVE: sieve
        } } },
        "apiUrl": "https://mail.test.local/jmap/",
        "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "https://mail.test.local/upload/{accountId}/"
    });
    let executor = FakeExecutor::from_session(&session, Vec::new());
    vocabulary(executor.session())
}

/// The extensions Stalwart lists that filing reads (`filters-and-out-of-office.md`, "Observed on
/// the harness").
const STALWART: &[&str] = &[
    "copy",
    "fileinto",
    "imap4flags",
    "include",
    "mailbox",
    "mailboxid",
    "special-use",
    "vacation",
];

fn folders() -> Vec<Mailbox> {
    fixture("mailbox_get.json")["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value: &Value| mailbox_from_json(value).unwrap())
        .collect()
}

/// The fixture's mailboxes and two more that share a role, which RFC 8621 §2 forbids and a
/// server may still answer.
fn folders_sharing_a_role() -> Vec<Mailbox> {
    let mut folders = folders();
    for id in ["x", "y"] {
        folders.push(
            mailbox_from_json(
                &json!({ "id": id, "name": id, "parentId": null, "role": "archive" }),
            )
            .unwrap(),
        );
    }
    folders
}

/// The rule's folder as the adapter reads back the script its writer wrote for a rule filing
/// into `mailbox`.
fn round_trip(mailbox: RuleMailbox, vocabulary: &RuleVocabulary) -> RuleMailbox {
    let rule = MailRule::typed(
        Match::default(),
        vec![Action::FileInto {
            mailbox,
            copy: false,
        }],
    );
    let plan = plan_rules(&[rule], vocabulary, PlanMode::Edit);
    let text = sieve_scripts::compose("", &plan.written(), false).expect("written");
    let read = sieve_scripts::user_rules(&text);
    let [rule] = read.as_slice() else {
        panic!("one rule read back");
    };
    let RuleBody::Typed(typed) = &rule.body else {
        panic!("read typed");
    };
    let [Action::FileInto { mailbox, .. }] = typed.actions.as_slice() else {
        panic!("one filing read back");
    };
    mailbox.clone()
}

/// Each mailbox's name on `vocabulary`, after checking it reads back as the same mailbox.
fn names(vocabulary: &RuleVocabulary) -> Vec<(String, Option<RuleMailbox>)> {
    names_among(&folders(), vocabulary)
}

/// Each of `folders`' names on `vocabulary`, after checking it reads back as the same mailbox.
fn names_among(
    folders: &[Mailbox],
    vocabulary: &RuleVocabulary,
) -> Vec<(String, Option<RuleMailbox>)> {
    folders
        .iter()
        .map(|folder| {
            let named = RuleMailbox::for_folder(folder, folders, vocabulary);
            if let Some(named) = &named {
                let read = round_trip(named.clone(), vocabulary);
                assert_eq!(
                    read.folder_in(folders, vocabulary).map(|f| &f.id),
                    Some(&folder.id),
                    "{named:?} read back as {read:?}"
                );
            }
            (folder.id.as_str().to_owned(), named)
        })
        .collect()
}

fn id(id: &str) -> RuleMailbox {
    RuleMailbox::Id(engine_core::ids::MailboxId::try_from(id).unwrap())
}

fn role(role: MailboxRole) -> RuleMailbox {
    RuleMailbox::Role(role)
}

#[test]
fn on_stalwart_a_role_is_filed_by_role_and_any_other_mailbox_by_id() {
    assert_eq!(
        names(&vocabulary_listing(STALWART)),
        [
            ("b".to_owned(), Some(role(MailboxRole::Trash))),
            ("i".to_owned(), Some(id("i"))),
            ("h".to_owned(), Some(id("h"))),
            ("a".to_owned(), Some(role(MailboxRole::Inbox))),
            ("d".to_owned(), Some(role(MailboxRole::Drafts))),
            ("e".to_owned(), Some(role(MailboxRole::Sent))),
            ("c".to_owned(), Some(role(MailboxRole::Junk))),
        ]
    );
}

#[test]
fn without_mailboxid_or_special_use_only_the_inbox_has_a_name() {
    // A mailbox's name chain is no path the engine has proven as a Sieve `fileinto` argument.
    let bare = names(&vocabulary_listing(&["fileinto", "copy"]));
    let named: Vec<&str> = bare
        .iter()
        .filter(|(_, named)| named.is_some())
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(named, ["a"]);
    // With `special-use` alone, the mailboxes with a role too.
    let by_role = names(&vocabulary_listing(&["fileinto", "special-use"]));
    assert_eq!(by_role[0], ("b".to_owned(), Some(role(MailboxRole::Trash))));
    assert_eq!(by_role[1], ("i".to_owned(), None));
}

#[test]
fn mailboxes_sharing_a_role_are_filed_by_id() {
    let names = names_among(&folders_sharing_a_role(), &vocabulary_listing(STALWART));
    assert_eq!(names[0], ("b".to_owned(), Some(role(MailboxRole::Trash))));
    assert_eq!(names[7], ("x".to_owned(), Some(id("x"))));
    assert_eq!(names[8], ("y".to_owned(), Some(id("y"))));
}
