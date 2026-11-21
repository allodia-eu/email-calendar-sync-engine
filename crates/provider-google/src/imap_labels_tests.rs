use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{Keyword, Message},
    membership::Memberships,
};
use engine_provider::KeywordName;
use provider_imap::{GmailFolder, GmailMessage, GmailState};
use serde_json::json;

use super::*;
use crate::normalize::message_from_json;

fn ids() -> LabelIds {
    LabelIds::from_list(&[
        json!({ "id": "INBOX", "name": "INBOX" }),
        json!({ "id": "Label_7", "name": "Reis/été" }),
    ])
}

fn state(labels: &[&str], flags: &[&str]) -> GmailState {
    GmailState {
        uid: 5,
        gmail_id: 0x1375_6731_6eaa_c854,
        thread_id: 0x1375_6731_6eaa_c800,
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        flags: flags.iter().map(|f| (*f).to_owned()).collect(),
        modseq: None,
    }
}

#[test]
fn a_label_is_named_back_as_imap_names_it() {
    assert_eq!(imap_label_name("SENT", &ids()).as_deref(), Some("\\Sent"));
    assert_eq!(imap_label_name("INBOX", &ids()).as_deref(), Some("\\Inbox"));
    assert_eq!(
        imap_label_name("Label_7", &ids()).as_deref(),
        Some("Reis/été")
    );
    assert_eq!(imap_label_name("Label_404", &ids()), None);
    assert_eq!(imap_label_name("ALL_MAIL", &ids()), None);
}

#[test]
fn imap_says_in_three_places_what_the_api_says_in_one() {
    let got = label_ids(
        &state(
            &["\\Inbox", "\\Important", "Reis/été", "Nobody knows me"],
            &[],
        ),
        GmailFolder::All,
        &ids(),
    );
    assert_eq!(got, ["IMPORTANT", "INBOX", "Label_7", "UNREAD"]);

    let read_and_starred = label_ids(
        &state(&[], &["\\Seen", "\\Flagged"]),
        GmailFolder::Trash,
        &ids(),
    );
    assert_eq!(read_and_starred, ["STARRED", "TRASH"]);

    let spam = label_ids(&state(&[], &["\\Seen"]), GmailFolder::Spam, &ids());
    assert_eq!(spam, ["SPAM"]);
}

#[test]
fn a_message_read_over_imap_is_the_one_the_api_reads() {
    let api = message_from_json(&json!({
        "id": "137567316eaac854",
        "threadId": "137567316eaac800",
        "labelIds": ["INBOX", "Label_7", "UNREAD", "STARRED", "CATEGORY_UPDATES"],
        "payload": { "headers": [{ "name": "Date", "value": "Thu, 1 Oct 2026 10:00:00 +0000" }] }
    }))
    .unwrap();

    // The API lists the inbox category and IMAP cannot: both drop it, so the two still agree.
    let placeholder = MessageId::new(ProviderKey::new("imap:v1:u5@[Gmail]/All Mail").unwrap());
    let mut message = Message::new(
        placeholder,
        Memberships::of_one(MailboxId::try_from("[Gmail]/All Mail").unwrap()),
    );
    // What the IMAP read made of the same `Date` header, which the mapping must keep.
    message.sent_at = engine_mime::date::parse("Thu, 1 Oct 2026 10:00:00 +0000");
    let read = GmailMessage {
        state: state(&["\\Inbox", "Reis/été"], &["\\Flagged"]),
        message,
    };
    let imap = message_of(read, GmailFolder::All, &ids(), &NamedLabels::default()).unwrap();

    assert_eq!(imap.id, api.id);
    assert_eq!(imap.thread, api.thread);
    assert_eq!(imap.mailboxes, api.mailboxes);
    assert_eq!(imap.keywords, api.keywords);
    assert!(api.sent_at.is_some());
    assert_eq!(imap.sent_at, api.sent_at);
}

#[test]
fn an_archived_message_lands_in_all_mail_as_on_the_api() {
    let change = state_change_of(
        &state(&[], &["\\Seen"]),
        GmailFolder::All,
        &ids(),
        &NamedLabels::default(),
    )
    .unwrap();
    assert_eq!(change.key.as_str(), "137567316eaac854");
    let places: Vec<&str> = change
        .state
        .mailboxes
        .as_ref()
        .unwrap()
        .iter()
        .map(MailboxId::as_str)
        .collect();
    assert_eq!(places, ["ALL_MAIL"]);
}

fn keyword() -> Keyword {
    Keyword::new("project-x").unwrap()
}

/// The captured label list, and the keyword's label as Gmail answered its create: hidden from
/// the label list (`Label_6`, `Fixture keyword label`).
fn listed() -> Vec<serde_json::Value> {
    let captured: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/mail/labels.json")).unwrap();
    let mut labels = captured["labels"].as_array().unwrap().clone();
    labels.push(
        serde_json::from_str(include_str!(
            "../tests/fixtures/mail/label_created_hidden.json"
        ))
        .unwrap(),
    );
    labels
}

/// The keyword under a name of another device's language, and the label's own name in another
/// case.
fn named() -> NamedLabels {
    let names = [KeywordName::new(keyword(), "Project X")
        .unwrap()
        .also_known_as("FIXTURE KEYWORD LABEL")];
    NamedLabels::from_list(&listed(), &names)
}

fn read(labels: &[&str]) -> GmailMessage {
    let placeholder = MessageId::new(ProviderKey::new("imap:v1:u5@[Gmail]/All Mail").unwrap());
    GmailMessage {
        state: state(labels, &["\\Seen"]),
        message: Message::new(
            placeholder,
            Memberships::of_one(MailboxId::try_from("[Gmail]/All Mail").unwrap()),
        ),
    }
}

fn places_of(mailboxes: &Memberships<MailboxId>) -> Vec<&str> {
    mailboxes.iter().map(MailboxId::as_str).collect()
}

#[test]
fn a_named_label_imap_lists_is_the_keyword_and_not_a_folder_as_on_the_api() {
    let ids = LabelIds::from_list(&listed());
    // A label under another name stays a folder, and so does one nested under it.
    let labels = [
        "\\Sent",
        "Fixture keyword label",
        "Fixture Label",
        "Fixture Label/Nested",
    ];
    let imap = message_of(read(&labels), GmailFolder::All, &ids, &named()).unwrap();
    assert!(imap.keywords.contains(&keyword()), "{:?}", imap.keywords);
    assert_eq!(places_of(&imap.mailboxes), ["Label_1", "Label_2", "SENT"]);

    let mut api = message_from_json(&json!({
        "id": "137567316eaac854",
        "threadId": "137567316eaac800",
        "labelIds": ["SENT", "Label_6", "Label_1", "Label_2"],
    }))
    .unwrap();
    named().apply(&mut api);
    assert_eq!(imap.mailboxes, api.mailboxes);
    assert_eq!(imap.keywords, api.keywords);

    let unnamed = message_of(
        read(&labels),
        GmailFolder::All,
        &ids,
        &NamedLabels::default(),
    )
    .unwrap();
    assert!(!unnamed.keywords.contains(&keyword()));
    assert!(
        places_of(&unnamed.mailboxes).contains(&"Label_6"),
        "without a name it is a label"
    );
}

#[test]
fn a_change_imap_reports_carries_the_keyword_and_a_message_filed_nowhere_else_is_in_all_mail() {
    let ids = LabelIds::from_list(&listed());
    let sent = state_change_of(
        &state(&["\\Sent", "Fixture keyword label"], &["\\Seen"]),
        GmailFolder::All,
        &ids,
        &named(),
    )
    .unwrap();
    assert!(sent.state.keywords.contains(&keyword()));
    assert_eq!(places_of(sent.state.mailboxes.as_ref().unwrap()), ["SENT"]);

    let alone = state_change_of(
        &state(&["Fixture keyword label"], &["\\Seen"]),
        GmailFolder::All,
        &ids,
        &named(),
    )
    .unwrap();
    assert!(alone.state.keywords.contains(&keyword()));
    assert_eq!(
        places_of(alone.state.mailboxes.as_ref().unwrap()),
        ["ALL_MAIL"]
    );

    let archived = message_of(
        read(&["Fixture keyword label"]),
        GmailFolder::All,
        &ids,
        &named(),
    )
    .unwrap();
    assert!(archived.keywords.contains(&keyword()));
    assert_eq!(places_of(&archived.mailboxes), ["ALL_MAIL"]);
}
