//! A rule's folder over ManageSieve, end to end offline: named from the account's folders as the
//! adapter lists them (`LIST`, modified UTF-7 on IMAP4rev1), written by the adapter's writer, read
//! back by its reader, and resolved to the folder it was named from, on what each harness server
//! lists.

use engine_core::{
    mail::{Mailbox, MailboxRole},
    rules::{Action, MailRule, MailboxPath, Match, RuleBody, RuleMailbox},
};

use super::*;
use crate::{mail::mailboxes_from_list, mock::MockStream, parse::parse_list};

/// The vocabulary of an account on the server whose TLS greeting is `greeting`.
async fn vocabulary_of(greeting: &[u8]) -> RuleVocabulary {
    let (stream, _) = MockStream::new(greeting.to_vec());
    let session = Session::open(stream).await.unwrap();
    vocabulary(session.capabilities())
}

/// An account's folders, as the adapter lists them.
fn folders() -> Vec<Mailbox> {
    let lines: Vec<Vec<u8>> = [
        r#"LIST (\HasNoChildren) "/" INBOX"#,
        r#"LIST (\HasNoChildren \Trash) "/" Trash"#,
        r#"LIST (\HasNoChildren \Sent) "/" Sent"#,
        r#"LIST (\HasChildren) "/" Archive"#,
        r#"LIST (\HasNoChildren) "/" Archive/2024"#,
        r#"LIST (\HasNoChildren) "/" "Re&AOc-us""#,
        r#"LIST (\Noselect \HasChildren) "/" Levels"#,
        r#"LIST (\HasNoChildren) "/" Levels/Leaf"#,
        r#"LIST (\HasNoChildren \Archive) "/" Old"#,
        r#"LIST (\HasNoChildren \Archive) "/" Older"#,
        r#"LIST (\HasNoChildren \All) "/" "All Mail""#,
    ]
    .iter()
    .map(|line| line.as_bytes().to_vec())
    .collect();
    let rows = parse_list(&lines).unwrap();
    mailboxes_from_list(&rows, true)
        .into_iter()
        .map(|(_, mailbox)| mailbox)
        .collect()
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
    let text = compose("", &plan.written(), None, false).expect("written");
    let state = State {
        listed: Vec::new(),
        script: None,
        text,
        server_reply: None,
    };
    let read = state.rules();
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

/// Each folder's name on `vocabulary`, after checking it reads back as the same folder.
fn names(vocabulary: &RuleVocabulary) -> Vec<(String, Option<RuleMailbox>)> {
    let folders = folders();
    folders
        .iter()
        .map(|folder| {
            let named = RuleMailbox::for_folder(folder, &folders, vocabulary);
            if let Some(named) = &named {
                let read = round_trip(named.clone(), vocabulary);
                assert_eq!(
                    read.folder_in(&folders, vocabulary).map(|f| &f.id),
                    Some(&folder.id),
                    "{named:?} read back as {read:?}"
                );
            }
            (folder.id.as_str().to_owned(), named)
        })
        .collect()
}

fn path(path: &str) -> RuleMailbox {
    RuleMailbox::Path(MailboxPath::new(path).unwrap())
}

#[tokio::test]
async fn on_dovecot_every_folder_is_filed_by_its_path_and_read_back_as_itself() {
    let vocabulary = vocabulary_of(include_bytes!(
        "../../tests/fixtures/managesieve/dovecot_greeting_tls.txt"
    ))
    .await;
    // No `special-use`: a role is filed by the folder's path, the inbox by its name.
    assert_eq!(
        names(&vocabulary),
        [
            (
                "INBOX".to_owned(),
                Some(RuleMailbox::Role(MailboxRole::Inbox))
            ),
            ("Trash".to_owned(), Some(path("Trash"))),
            ("Sent".to_owned(), Some(path("Sent"))),
            ("Archive".to_owned(), Some(path("Archive"))),
            ("Archive/2024".to_owned(), Some(path("Archive/2024"))),
            ("Reçus".to_owned(), Some(path("Reçus"))),
            ("Levels".to_owned(), None),
            ("Levels/Leaf".to_owned(), Some(path("Levels/Leaf"))),
            ("Old".to_owned(), Some(path("Old"))),
            ("Older".to_owned(), Some(path("Older"))),
            ("All Mail".to_owned(), None),
        ]
    );
}

#[tokio::test]
async fn on_stalwart_a_role_one_folder_holds_is_filed_by_role_and_none_by_id() {
    let vocabulary = vocabulary_of(include_bytes!(
        "../../tests/fixtures/managesieve/stalwart_greeting_tls.txt"
    ))
    .await;
    let names = names(&vocabulary);
    assert_eq!(
        names[1],
        (
            "Trash".to_owned(),
            Some(RuleMailbox::Role(MailboxRole::Trash))
        )
    );
    assert_eq!(names[3], ("Archive".to_owned(), Some(path("Archive"))));
    // Two folders share `\Archive`: the server would choose between them, so each by its path.
    assert_eq!(names[8], ("Old".to_owned(), Some(path("Old"))));
    assert_eq!(names[9], ("Older".to_owned(), Some(path("Older"))));
    assert_eq!(
        names[10],
        ("All Mail".to_owned(), None),
        "a virtual collection"
    );
    assert!(
        names
            .iter()
            .all(|(_, named)| !matches!(named, Some(RuleMailbox::Id(_)))),
        "the server lists `mailboxid`, and a folder id here is no MAILBOXID"
    );
}
