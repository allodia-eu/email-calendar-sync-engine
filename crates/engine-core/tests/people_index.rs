//! Conservative unified-people derivation contracts.

use engine_core::{
    contact::{
        ContactCard, ContactEmail, ContactName, ContactPhone, ContactProperty, ContactSourceClass,
        Organization, PropertyId, Title,
    },
    ids::{AccountId, AddressBookId, ContactId, PersonId},
    membership::Memberships,
    people::{CanonicalEmail, PeopleSnapshot, PersonSource, PersonSourceId, rebuild_people},
};

fn source(
    account: &str,
    contact: &str,
    name: Option<&str>,
    emails: &[&str],
    class: ContactSourceClass,
    writable: bool,
) -> PersonSource {
    let mut card = ContactCard::new(
        ContactId::try_from(contact).unwrap(),
        Memberships::of_one(AddressBookId::try_from("book").unwrap()),
    );
    card.name = name.map(|full| ContactName {
        full: Some(full.into()),
        ..ContactName::default()
    });
    for (index, email) in emails.iter().enumerate() {
        card.emails.insert(
            PropertyId::new(format!("e{index}")).unwrap(),
            ContactProperty::new(ContactEmail::new(*email)),
        );
    }
    PersonSource::new(AccountId::try_from(account).unwrap(), card, class, writable)
}

#[test]
fn canonical_email_only_folds_the_idna_domain() {
    assert_eq!(
        CanonicalEmail::parse("  Local.Part@例え.テスト ")
            .unwrap()
            .as_str(),
        "Local.Part@xn--r8jz45g.xn--zckzah"
    );
    assert_ne!(
        CanonicalEmail::parse("Local@EXAMPLE.test").unwrap(),
        CanonicalEmail::parse("local@example.test").unwrap()
    );
    assert!(CanonicalEmail::parse("not-an-address").is_err());
}

#[test]
fn exact_email_connected_components_merge_transitively() {
    let sources = vec![
        source(
            "a",
            "one",
            Some("One"),
            &["shared@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "b",
            "two",
            Some("Two"),
            &["shared@example.test", "bridge@example.test"],
            ContactSourceClass::Suggested,
            false,
        ),
        source(
            "c",
            "three",
            Some("Three"),
            &["bridge@example.test"],
            ContactSourceClass::Directory,
            false,
        ),
    ];

    let built = rebuild_people(&sources, &PeopleSnapshot::empty()).unwrap();
    assert_eq!(built.people.len(), 1);
    assert_eq!(built.people[0].sources.len(), 3);
    assert_eq!(built.people[0].display_name, "One");
    assert_eq!(built.people[0].emails.len(), 2);
}

#[test]
fn local_part_case_and_no_email_cards_do_not_false_merge() {
    let sources = vec![
        source(
            "a",
            "upper",
            Some("Upper"),
            &["Case@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "a",
            "lower",
            Some("Lower"),
            &["case@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "a",
            "no-email-a",
            Some("Same Name"),
            &[],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "b",
            "no-email-b",
            Some("Same Name"),
            &[],
            ContactSourceClass::Personal,
            true,
        ),
    ];
    let built = rebuild_people(&sources, &PeopleSnapshot::empty()).unwrap();
    assert_eq!(built.people.len(), 4);
}

#[test]
fn merge_keeps_oldest_id_and_records_alias() {
    let separate = vec![
        source(
            "a",
            "one",
            Some("One"),
            &["one@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "b",
            "two",
            Some("Two"),
            &["two@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
    ];
    let first = rebuild_people(&separate, &PeopleSnapshot::empty()).unwrap();
    assert_eq!(first.people.len(), 2);
    let oldest = first.people[0].id.min(first.people[1].id);
    let retired = first.people[0].id.max(first.people[1].id);

    let mut merged_sources = separate;
    merged_sources[1].card.emails.insert(
        PropertyId::new("bridge").unwrap(),
        ContactProperty::new(ContactEmail::new("one@example.test")),
    );
    let merged = rebuild_people(&merged_sources, &first).unwrap();
    assert_eq!(merged.people.len(), 1);
    assert_eq!(merged.people[0].id, oldest);
    assert_eq!(merged.aliases.get(&retired), Some(&oldest));
}

#[test]
fn split_retains_old_id_deterministically_and_mints_another() {
    let joined = vec![
        source(
            "a",
            "one",
            Some("One"),
            &["shared@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "b",
            "two",
            Some("Two"),
            &["shared@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
    ];
    let first = rebuild_people(&joined, &PeopleSnapshot::empty()).unwrap();
    let old_id = first.people[0].id;

    let split = vec![
        source(
            "a",
            "one",
            Some("One"),
            &["one@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
        source(
            "b",
            "two",
            Some("Two"),
            &["two@example.test"],
            ContactSourceClass::Personal,
            true,
        ),
    ];
    let rebuilt = rebuild_people(&split, &first).unwrap();
    assert_eq!(rebuilt.people.len(), 2);
    assert_eq!(rebuilt.people[0].id, old_id);
    assert!(rebuilt.people[1].id > old_id);
}

/// Shared canonical email is the only join signal. Two email-less cards — even with
/// identical display names — stay separate people: no provider supplies a stable
/// cross-source person handle, and joining on a name would merge distinct people who
/// happen to share one.
#[test]
fn email_less_cards_do_not_join() {
    let one = source(
        "a",
        "one",
        Some("Ada Lovelace"),
        &[],
        ContactSourceClass::Directory,
        false,
    );
    let two = source(
        "a",
        "two",
        Some("Ada Lovelace"),
        &[],
        ContactSourceClass::Personal,
        true,
    );

    let built = rebuild_people(&[one, two], &PeopleSnapshot::empty()).unwrap();
    assert_eq!(built.people.len(), 2);
}

/// The converse: a shared canonical email *does* join, across source classes, and the
/// personal record wins the display name. Only the domain folds — the local part is
/// case-sensitive (RFC 5321), so it must match exactly.
#[test]
fn a_shared_canonical_email_joins_across_source_classes() {
    let one = source(
        "a",
        "one",
        Some("Directory Record"),
        &["ada@Example.TEST"],
        ContactSourceClass::Directory,
        false,
    );
    let two = source(
        "a",
        "two",
        Some("Personal Record"),
        &["ada@example.test"],
        ContactSourceClass::Personal,
        true,
    );

    let built = rebuild_people(&[one, two], &PeopleSnapshot::empty()).unwrap();
    assert_eq!(built.people.len(), 1);
    assert_eq!(built.people[0].display_name, "Personal Record");
}

#[test]
fn provenance_names_the_source_record() {
    let src = source(
        "account",
        "contact",
        Some("Saved Name"),
        &["saved@example.test"],
        ContactSourceClass::Personal,
        true,
    );
    let built = rebuild_people(&[src], &PeopleSnapshot::empty()).unwrap();
    assert_eq!(
        built.people[0].names[0].sources,
        [PersonSourceId::new(
            AccountId::try_from("account").unwrap(),
            ContactId::try_from("contact").unwrap()
        )]
        .into()
    );
    assert_eq!(built.people[0].id, PersonId::new(1).unwrap());
}

#[test]
fn materialized_people_union_phone_organization_title_and_fallback_names() {
    let mut src = source(
        "account",
        "contact",
        None,
        &["fallback@example.test", "invalid"],
        ContactSourceClass::Personal,
        false,
    );
    src.card.phones.insert(
        PropertyId::new("phone").unwrap(),
        ContactProperty::new(ContactPhone {
            number: "+31 20 555 0100".into(),
            ..ContactPhone::default()
        }),
    );
    src.card.organizations.insert(
        PropertyId::new("organization").unwrap(),
        ContactProperty::new(Organization {
            name: "Example BV".into(),
            ..Organization::default()
        }),
    );
    src.card.titles.insert(
        PropertyId::new("title").unwrap(),
        ContactProperty::new(Title {
            name: "Engineer".into(),
            ..Title::default()
        }),
    );

    let built = rebuild_people(&[src], &PeopleSnapshot::empty()).unwrap();
    let person = &built.people[0];
    assert_eq!(person.display_name, "fallback@example.test");
    assert_eq!(person.emails.len(), 1);
    assert_eq!(person.phones[0].value, "+31 20 555 0100");
    assert_eq!(person.organizations[0].value, "Example BV");
    assert_eq!(person.titles[0].value, "Engineer");
    assert!(person.is_saved);
    assert!(!person.is_writable);
}

#[test]
fn an_email_less_nameless_card_gets_the_explicit_unnamed_fallback() {
    let built = rebuild_people(
        &[source(
            "account",
            "contact",
            None,
            &[],
            ContactSourceClass::MailHistory,
            false,
        )],
        &PeopleSnapshot::empty(),
    )
    .unwrap();
    assert_eq!(built.people[0].display_name, "Unnamed contact");
}

#[test]
fn snapshot_resolves_alias_chains_and_rejects_cycles_or_unknown_ids() {
    let source = source(
        "account",
        "contact",
        Some("Current"),
        &["current@example.test"],
        ContactSourceClass::Personal,
        true,
    );
    let mut snapshot = rebuild_people(&[source], &PeopleSnapshot::empty()).unwrap();
    let current = snapshot.people[0].id;
    let middle = PersonId::new(2).unwrap();
    let retired = PersonId::new(3).unwrap();
    snapshot.aliases.insert(middle, current);
    snapshot.aliases.insert(retired, middle);
    assert_eq!(
        snapshot.resolve(retired).map(|person| person.id),
        Some(current)
    );
    assert_eq!(snapshot.resolve(PersonId::new(99).unwrap()), None);

    let cycle_a = PersonId::new(4).unwrap();
    let cycle_b = PersonId::new(5).unwrap();
    snapshot.aliases.insert(cycle_a, cycle_b);
    snapshot.aliases.insert(cycle_b, cycle_a);
    assert_eq!(snapshot.resolve(cycle_a), None);
}

#[test]
fn source_priority_is_deterministic_across_every_authority_class() {
    let sources = vec![
        source(
            "d",
            "directory",
            Some("Directory"),
            &["shared@example.test"],
            ContactSourceClass::Directory,
            true,
        ),
        source(
            "m",
            "history",
            Some("History"),
            &["shared@example.test"],
            ContactSourceClass::MailHistory,
            false,
        ),
        source(
            "s",
            "suggested",
            Some("Suggested"),
            &["shared@example.test"],
            ContactSourceClass::Suggested,
            true,
        ),
        source(
            "p",
            "personal",
            Some("Personal"),
            &["shared@example.test"],
            ContactSourceClass::Personal,
            false,
        ),
    ];
    let built = rebuild_people(&sources, &PeopleSnapshot::empty()).unwrap();
    assert_eq!(built.people[0].display_name, "Personal");
}
