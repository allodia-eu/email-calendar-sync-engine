//! What a queued folder change comes to against the server's current folder list.

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::{Mailbox, MailboxRole},
};
use engine_provider::MailboxEdit;

use super::{MailboxChange, MailboxNameError, MailboxPlace, Plan, plan, validate_mailbox_name};

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

fn folder(key: &str, name: &str, parent: Option<&str>) -> Mailbox {
    let mut mailbox = Mailbox::new(id(key), name);
    mailbox.parent = parent.map(id);
    mailbox
}

fn with_role(mut mailbox: Mailbox, role: MailboxRole) -> Mailbox {
    mailbox.role = Some(role);
    mailbox
}

/// Inbox, Trash (holding a `Bills`), and `Work` holding `2024`, which holds `Q1`.
fn tree() -> Vec<Mailbox> {
    vec![
        with_role(folder("inbox", "INBOX", None), MailboxRole::Inbox),
        with_role(folder("trash", "Trash", None), MailboxRole::Trash),
        folder("trash-bills", "Bills", Some("trash")),
        folder("work", "Work", None),
        folder("w2024", "2024", Some("work")),
        folder("q1", "Q1", Some("w2024")),
        folder("bills", "Bills", None),
    ]
}

fn seen(name: &str, parent: Option<&str>) -> MailboxPlace {
    MailboxPlace {
        name: name.into(),
        parent: parent.map(id),
    }
}

fn class(change: &MailboxChange) -> FailureClass {
    plan(change, &tree()).expect_err("refused").class()
}

#[test]
fn a_new_folder_is_sent_and_one_already_there_is_done() {
    let fresh = MailboxChange::Create {
        name: "Receipts".into(),
        parent: Some(id("work")),
    };
    assert_eq!(
        plan(&fresh, &tree()).unwrap(),
        Plan::Send(MailboxEdit::Create {
            name: "Receipts".into(),
            parent: Some(id("work")),
        })
    );
    // A retry after the first attempt landed meets the folder it made.
    let again = MailboxChange::Create {
        name: "2024".into(),
        parent: Some(id("work")),
    };
    assert_eq!(
        plan(&again, &tree()).unwrap(),
        Plan::Done(Some(id("w2024")))
    );
}

#[test]
fn a_folder_cannot_be_made_inside_one_that_is_gone() {
    let orphan = MailboxChange::Create {
        name: "Receipts".into(),
        parent: Some(id("gone")),
    };
    assert_eq!(class(&orphan), FailureClass::Conflict);
}

#[test]
fn a_rename_is_sent_when_the_folder_is_where_the_user_saw_it() {
    let rename = MailboxChange::Update {
        target: id("w2024"),
        seen: seen("2024", Some("work")),
        name: "2025".into(),
        parent: None,
    };
    assert_eq!(
        plan(&rename, &tree()).unwrap(),
        Plan::Send(MailboxEdit::Update {
            target: id("w2024"),
            name: "2025".into(),
            parent: None,
        })
    );
}

#[test]
fn a_change_made_elsewhere_while_queued_is_a_conflict_not_an_overwrite() {
    let mut elsewhere = tree();
    elsewhere
        .iter_mut()
        .find(|m| m.id == id("w2024"))
        .unwrap()
        .name = "Archive 2024".into();
    let rename = MailboxChange::Update {
        target: id("w2024"),
        seen: seen("2024", Some("work")),
        name: "2025".into(),
        parent: Some(id("work")),
    };
    let err = plan(&rename, &elsewhere).expect_err("the server copy moved on");
    assert_eq!(err.class(), FailureClass::Conflict);

    let trash = MailboxChange::Trash {
        target: id("w2024"),
        seen: seen("2024", Some("work")),
        trash: id("trash"),
    };
    assert_eq!(
        plan(&trash, &elsewhere).unwrap_err().class(),
        FailureClass::Conflict
    );
}

#[test]
fn a_folder_that_is_gone_cannot_be_changed_but_is_already_deleted() {
    let rename = MailboxChange::Update {
        target: id("gone"),
        seen: seen("Gone", None),
        name: "Back".into(),
        parent: None,
    };
    assert_eq!(class(&rename), FailureClass::Conflict);
    let delete = MailboxChange::Delete { target: id("gone") };
    assert_eq!(plan(&delete, &tree()).unwrap(), Plan::Done(None));
}

#[test]
fn a_folder_cannot_move_inside_itself() {
    for into in ["work", "w2024", "q1"] {
        let change = MailboxChange::Update {
            target: id("work"),
            seen: seen("Work", None),
            name: "Work".into(),
            parent: Some(id(into)),
        };
        assert_eq!(class(&change), FailureClass::Conflict, "into {into}");
    }
}

#[test]
fn a_move_onto_a_name_already_taken_is_a_conflict() {
    // `Bills` at the top level already exists beside `Work`.
    let change = MailboxChange::Update {
        target: id("w2024"),
        seen: seen("2024", Some("work")),
        name: "Bills".into(),
        parent: None,
    };
    assert_eq!(class(&change), FailureClass::Conflict);
}

#[test]
fn a_change_that_changes_nothing_is_done() {
    let same = MailboxChange::Update {
        target: id("w2024"),
        seen: seen("2024", Some("work")),
        name: "2024".into(),
        parent: Some(id("work")),
    };
    assert_eq!(plan(&same, &tree()).unwrap(), Plan::Done(Some(id("w2024"))));
}

#[test]
fn a_trashed_folder_takes_a_name_trash_does_not_hold() {
    let change = MailboxChange::Trash {
        target: id("bills"),
        seen: seen("Bills", None),
        trash: id("trash"),
    };
    assert_eq!(
        plan(&change, &tree()).unwrap(),
        Plan::Send(MailboxEdit::Trash {
            target: id("bills"),
            trash: id("trash"),
            name: "Bills 2".into(),
        })
    );
    let mut folded = tree();
    folded.push(folder("trash-bills-2", "bills 2", Some("trash")));
    let Plan::Send(MailboxEdit::Trash { name, .. }) = plan(&change, &folded).unwrap() else {
        panic!("expected a trash");
    };
    assert_eq!(name, "Bills 3", "a name differing only in case is taken");
}

#[test]
fn trash_and_the_inbox_are_refused() {
    let trash_itself = MailboxChange::Trash {
        target: id("trash"),
        seen: seen("Trash", None),
        trash: id("trash"),
    };
    assert_eq!(class(&trash_itself), FailureClass::InvalidState);
    let inbox = MailboxChange::Update {
        target: id("inbox"),
        seen: seen("INBOX", None),
        name: "Old inbox".into(),
        parent: None,
    };
    assert_eq!(class(&inbox), FailureClass::InvalidState);
    let delete_inbox = MailboxChange::Delete {
        target: id("inbox"),
    };
    assert_eq!(class(&delete_inbox), FailureClass::InvalidState);
}

#[test]
fn a_cycle_the_server_reports_does_not_hang_the_walk() {
    let looped = vec![
        folder("c", "C", Some("d")),
        folder("d", "D", Some("c")),
        folder("x", "X", None),
    ];
    let change = MailboxChange::Update {
        target: id("x"),
        seen: seen("X", None),
        name: "X".into(),
        parent: Some(id("c")),
    };
    assert!(matches!(plan(&change, &looped), Ok(Plan::Send(_))));
}

#[test]
fn names_every_transport_refuses_are_caught_before_queueing() {
    assert_eq!(validate_mailbox_name("Receipts"), Ok(()));
    assert_eq!(validate_mailbox_name("Reçus 2024"), Ok(()));
    assert_eq!(validate_mailbox_name(""), Err(MailboxNameError::Empty));
    assert_eq!(validate_mailbox_name("   "), Err(MailboxNameError::Empty));
    assert_eq!(
        validate_mailbox_name(" Bills"),
        Err(MailboxNameError::Surrounded)
    );
    assert_eq!(
        validate_mailbox_name("Bi\tlls"),
        Err(MailboxNameError::Control)
    );
    assert_eq!(
        validate_mailbox_name("Work/2024"),
        Err(MailboxNameError::Separator)
    );
    let trash = MailboxChange::Trash {
        target: id("bills"),
        seen: seen("Bills", None),
        trash: id("trash"),
    };
    assert_eq!(trash.validate(), Ok(()));
    let rename = MailboxChange::Update {
        target: id("bills"),
        seen: seen("Bills", None),
        name: "a/b".into(),
        parent: None,
    };
    assert_eq!(rename.validate(), Err(MailboxNameError::Separator));
}
