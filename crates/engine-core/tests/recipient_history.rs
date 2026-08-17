//! Sent-recipient observation and suggestion-ranking contracts.

use std::collections::BTreeSet;

use engine_core::{
    contact::ContactKind,
    ids::{AccountId, ContactId, MailboxId, MessageId, PersonId},
    mail::{EmailAddress, Message},
    membership::Memberships,
    people::{CanonicalEmail, Person, PersonSourceId, SourcedValue},
    recipient::{RecipientInteraction, observe_sent_recipients, rank_recipient_suggestions},
};

fn sent_message() -> Message {
    let mut message = Message::new(
        MessageId::try_from("message-1").unwrap(),
        Memberships::new([
            MailboxId::try_from("archive").unwrap(),
            MailboxId::try_from("sent").unwrap(),
        ])
        .unwrap(),
    );
    message.sent_at = Some("2026-07-01T12:00:00Z".parse().unwrap());
    message.envelope.to = vec![
        EmailAddress::named("Alice", "alice@example.test"),
        EmailAddress::new("duplicate@example.test"),
    ];
    message.envelope.cc = vec![EmailAddress::new("duplicate@EXAMPLE.TEST")];
    message.envelope.bcc = vec![EmailAddress::named("Self", "me@example.test")];
    message
}

#[test]
fn sent_observations_include_to_cc_bcc_and_dedupe_per_message() {
    let message = sent_message();
    let observations = observe_sent_recipients(
        &AccountId::try_from("account").unwrap(),
        (&message).into(),
        message.mailboxes.iter(),
        &BTreeSet::from([MailboxId::try_from("sent").unwrap()]),
    );
    assert_eq!(observations.len(), 3);
    assert!(
        observations
            .iter()
            .any(|item| item.email.as_str() == "me@example.test")
    );
    assert_eq!(
        observations
            .iter()
            .filter(|item| item.email.as_str() == "duplicate@example.test")
            .count(),
        1
    );
}

#[test]
fn a_message_outside_sent_produces_no_observations() {
    let message = sent_message();
    let observations = observe_sent_recipients(
        &AccountId::try_from("account").unwrap(),
        (&message).into(),
        message.mailboxes.iter(),
        &BTreeSet::from([MailboxId::try_from("other-sent").unwrap()]),
    );
    assert!(observations.is_empty());
}

#[test]
fn suggestion_matching_precedes_recency_and_frequency() {
    let interactions = vec![
        RecipientInteraction::new(
            "alicia@example.test".parse().unwrap(),
            Some("Alicia".into()),
            100,
            Some("2026-07-22T12:00:00Z".parse().unwrap()),
        ),
        RecipientInteraction::new(
            "alice@example.test".parse().unwrap(),
            Some("Old Alice".into()),
            1,
            Some("2020-01-01T00:00:00Z".parse().unwrap()),
        ),
    ];
    let ranked = rank_recipient_suggestions("alice@example.test", &[], &interactions, 10);
    assert_eq!(ranked[0].email.as_str(), "alice@example.test");

    let empty = rank_recipient_suggestions("", &[], &interactions, 10);
    assert_eq!(empty[0].email.as_str(), "alicia@example.test");
}

fn person(id: u64, name: &str, email: &str, saved: bool, writable: bool) -> Person {
    let account = format!("account-{id}");
    let contact = format!("contact-{id}");
    let source = PersonSourceId::new(
        AccountId::try_from(account.as_str()).unwrap(),
        ContactId::try_from(contact.as_str()).unwrap(),
    );
    Person {
        id: PersonId::new(id).unwrap(),
        display_name: Some(name.into()),
        sources: BTreeSet::from([source.clone()]),
        kinds: BTreeSet::from([ContactKind::Individual]),
        names: vec![SourcedValue {
            value: name.into(),
            sources: BTreeSet::from([source.clone()]),
        }],
        emails: vec![SourcedValue {
            value: email.parse().unwrap(),
            sources: BTreeSet::from([source]),
        }],
        phones: Vec::new(),
        organizations: Vec::new(),
        titles: Vec::new(),
        is_saved: saved,
        is_writable: writable,
    }
}

#[test]
fn invalid_addresses_are_skipped_and_blank_names_are_not_observed() {
    let mut message = sent_message();
    message.envelope.to = vec![
        EmailAddress::named("   ", "valid@example.test"),
        EmailAddress::new("not-an-email"),
    ];
    message.envelope.cc.clear();
    message.envelope.bcc.clear();
    let observations = observe_sent_recipients(
        &AccountId::try_from("account").unwrap(),
        (&message).into(),
        message.mailboxes.iter(),
        &BTreeSet::from([MailboxId::try_from("sent").unwrap()]),
    );
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].name, None);
}

#[test]
fn suggestions_merge_contacts_and_history_by_email_without_losing_provenance() {
    let people = vec![
        person(1, "Saved Alice", "alice@example.test", true, true),
        person(2, "Directory Bob", "bob@example.test", false, false),
    ];
    let interactions = vec![
        RecipientInteraction::new(
            "alice@EXAMPLE.TEST".parse().unwrap(),
            Some("Observed Alice".into()),
            u64::MAX,
            Some("2026-07-20T12:00:00Z".parse().unwrap()),
        ),
        RecipientInteraction::new(
            "alice@example.test".parse().unwrap(),
            None,
            1,
            Some("2026-07-21T12:00:00Z".parse().unwrap()),
        ),
        RecipientInteraction::new(
            "history@example.test".parse().unwrap(),
            Some("History Only".into()),
            2,
            None,
        ),
    ];

    let ranked = rank_recipient_suggestions("saved", &people, &interactions, 10);
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].display_name, "Saved Alice");
    assert_eq!(ranked[0].sent_count, u64::MAX);
    assert!(ranked[0].is_saved);
    assert_eq!(ranked[0].provenance.len(), 1);
    assert_eq!(ranked[0].person_id, Some(PersonId::new(1).unwrap()));

    let history = rank_recipient_suggestions("history", &people, &interactions, 10);
    assert_eq!(history[0].display_name, "History Only");
    assert_eq!(history[0].person_id, None);
}

#[test]
fn suggestion_matching_covers_token_prefix_substring_priority_and_limit() {
    let people = vec![
        person(1, "Zed Alpha", "zeta@example.test", true, false),
        person(2, "Alpha", "other@example.test", false, true),
    ];
    let interactions = vec![RecipientInteraction::new(
        "plain@example.test".parse().unwrap(),
        None,
        5,
        None,
    )];

    let prefix = rank_recipient_suggestions("alp", &people, &interactions, 10);
    assert_eq!(prefix.len(), 2);
    assert_eq!(prefix[0].email.as_str(), "zeta@example.test");

    let substring = rank_recipient_suggestions("ain@", &people, &interactions, 10);
    assert_eq!(substring[0].display_name, "plain@example.test");

    let empty = rank_recipient_suggestions("", &people, &interactions, 2);
    assert_eq!(empty[0].email.as_str(), "plain@example.test");
    assert_eq!(empty.len(), 2);
    assert!(rank_recipient_suggestions("", &people, &interactions, 0).is_empty());
    assert!(rank_recipient_suggestions("missing", &people, &interactions, 10).is_empty());
}

#[test]
fn canonical_email_errors_and_serde_keep_the_conservative_key_valid() {
    for invalid in ["", "@example.test", "local@", "a@b@example.test"] {
        assert!(CanonicalEmail::parse(invalid).is_err(), "{invalid}");
    }
    assert!(CanonicalEmail::parse("local@xn--55555577").is_err());
    let email = CanonicalEmail::parse("Local@EXAMPLE.TEST").unwrap();
    let roundtrip: CanonicalEmail =
        serde_json::from_str(&serde_json::to_string(&email).unwrap()).unwrap();
    assert_eq!(roundtrip.as_str(), "Local@example.test");
    assert!(serde_json::from_str::<CanonicalEmail>("\"broken\"").is_err());
}
