use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::Message,
    membership::Memberships,
};
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
fn imap_says_in_three_places_what_the_api_says_in_one() {
    let mut categories = Categories::default();
    categories.add("CATEGORY_UPDATES", &[5]);
    let got = label_ids(&state(&["\\Inbox", "\\Important", "Reis/été", "Nobody knows me"], &[]), GmailFolder::All, &categories, &ids());
    assert_eq!(got, ["CATEGORY_UPDATES", "IMPORTANT", "INBOX", "Label_7", "UNREAD"]);

    let read_and_starred = label_ids(&state(&[], &["\\Seen", "\\Flagged"]), GmailFolder::Trash, &Categories::default(), &ids());
    assert_eq!(read_and_starred, ["STARRED", "TRASH"]);

    let spam = label_ids(&state(&[], &["\\Seen"]), GmailFolder::Spam, &Categories::default(), &ids());
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

    let mut categories = Categories::default();
    categories.add("CATEGORY_UPDATES", &[5]);
    let placeholder = MessageId::new(ProviderKey::new("imap:v1:u5@[Gmail]/All Mail").unwrap());
    let read = GmailMessage {
        state: state(&["\\Inbox", "Reis/été"], &["\\Flagged"]),
        message: Message::new(placeholder, Memberships::of_one(MailboxId::try_from("[Gmail]/All Mail").unwrap())),
        date: Some("Thu, 1 Oct 2026 10:00:00 +0000".to_owned()),
    };
    let imap = message_of(read, GmailFolder::All, &categories, &ids(), &NamedLabels::default()).unwrap();

    assert_eq!(imap.id, api.id);
    assert_eq!(imap.thread, api.thread);
    assert_eq!(imap.mailboxes, api.mailboxes);
    assert_eq!(imap.keywords, api.keywords);
    assert_eq!(imap.sent_at, api.sent_at);
}

#[test]
fn an_archived_uncategorised_message_lands_in_all_mail_as_on_the_api() {
    let change = state_change_of(&state(&[], &["\\Seen"]), GmailFolder::All, &Categories::default(), &ids(), &NamedLabels::default()).unwrap();
    assert_eq!(change.key.as_str(), "137567316eaac854");
    let places: Vec<&str> = change.state.mailboxes.as_ref().unwrap().iter().map(MailboxId::as_str).collect();
    assert_eq!(places, ["ALL_MAIL"]);
}
