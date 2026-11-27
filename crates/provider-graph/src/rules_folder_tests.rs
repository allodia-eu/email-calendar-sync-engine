//! A rule's folder on Graph, end to end offline: named from the account's folders as the adapter
//! lists them (`tests/fixtures/mail/`, a real mailbox's, roles from the well-known aliases in
//! `tests/fixtures/wellknown/`), written by the adapter's writer, read back by its reader, and
//! resolved to the folder it was named from.

use engine_core::{
    ids::MailboxId,
    mail::{Mailbox, MailboxRole},
    rules::{Action, MailRule, Match, RuleBody, RuleMailbox},
};
use serde_json::Value;

use super::*;
use crate::{
    normalize::{RECORD_ROLES, WELL_KNOWN_ROLES, apply_roles, folder_from_json},
    rules_read, rules_write,
};

/// The fixture `name`, where there is one.
fn read(name: &str) -> Option<Value> {
    let dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    let text = std::fs::read_to_string(format!("{dir}/tests/fixtures/{name}")).ok()?;
    serde_json::from_str(&text).ok()
}

fn fixture(name: &str) -> Value {
    read(name).unwrap()
}

/// Each well-known alias's folder id and role, as the adapter resolves them: an alias with no
/// fixture is one the mailbox does not answer.
fn resolved() -> Vec<(MailboxId, MailboxRole)> {
    WELL_KNOWN_ROLES
        .iter()
        .chain(RECORD_ROLES)
        .filter_map(|(alias, role)| {
            let doc = read(&format!("wellknown/{alias}.json"))?;
            Some((MailboxId::try_from(doc["id"].as_str()?).ok()?, role.clone()))
        })
        .collect()
}

/// The account's folders as the adapter lists them, with their roles.
fn folders() -> Vec<Mailbox> {
    let root = MailboxId::try_from(
        fixture("wellknown/msgfolderroot.json")["id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let mut folders: Vec<Mailbox> = ["mail/mailfolders.json", "mail/mailfolders_children.json"]
        .iter()
        .flat_map(|name| fixture(name)["value"].as_array().unwrap().clone())
        .map(|value| folder_from_json(&value, Some(&root)).unwrap())
        .collect();
    apply_roles(&mut folders, &resolved());
    folders
}

/// The rule's folder as the adapter reads back the rule its writer wrote for a rule filing into
/// `mailbox`, a role's folder resolved as the adapter resolves it.
fn round_trip(mailbox: RuleMailbox) -> RuleMailbox {
    let rule = MailRule::typed(
        Match::default(),
        vec![Action::FileInto {
            mailbox,
            copy: false,
        }],
    );
    let resolved = resolved();
    let folder = |role: &MailboxRole| {
        resolved
            .iter()
            .find(|(_, resolved)| resolved == role)
            .map(|(id, _)| id.clone())
    };
    let body = rules_write::body(&rule, &folder).expect("written");
    let read = rules_read::rule(&Value::Object(body), 0, 0);
    let RuleBody::Typed(typed) = &read.body else {
        panic!("read typed");
    };
    let [Action::FileInto { mailbox, .. }] = typed.actions.as_slice() else {
        panic!("one filing read back: {:?}", typed.actions);
    };
    mailbox.clone()
}

#[test]
fn every_folder_is_filed_by_role_or_by_id_and_read_back_as_itself() {
    let vocabulary = vocabulary();
    let folders = folders();
    let mut named = Vec::new();
    for folder in &folders {
        let mailbox =
            RuleMailbox::for_folder(folder, &folders, &vocabulary).expect("every folder is real");
        let read = round_trip(mailbox.clone());
        assert_eq!(
            read.folder_in(&folders, &vocabulary).map(|f| &f.id),
            Some(&folder.id),
            "{mailbox:?} read back as {read:?}"
        );
        named.push((folder.name.clone(), mailbox));
    }
    let role = |role| RuleMailbox::Role(role);
    let id = |folder: &str| {
        let folder = folders.iter().find(|f| f.name == folder).unwrap();
        RuleMailbox::Id(folder.id.clone())
    };
    assert_eq!(
        named,
        [
            ("Archiveren".to_owned(), role(MailboxRole::Archive)),
            ("Concepten".to_owned(), role(MailboxRole::Drafts)),
            ("Gesprekgeschiedenis".to_owned(), id("Gesprekgeschiedenis")),
            ("Ongewenste e-mail".to_owned(), role(MailboxRole::Junk)),
            // A rule names the inbox by role only where it files by name; Graph files by id.
            ("Postvak IN".to_owned(), id("Postvak IN")),
            ("Postvak UIT".to_owned(), id("Postvak UIT")),
            ("Verwijderde items".to_owned(), role(MailboxRole::Trash)),
            ("Verzonden items".to_owned(), role(MailboxRole::Sent)),
            ("Fixture parent".to_owned(), id("Fixture parent")),
        ]
    );
}
