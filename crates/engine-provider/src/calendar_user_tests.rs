use super::*;

#[test]
fn keeps_mailto_addresses_in_order_and_drops_other_schemes() {
    let set = CalendarUserAddresses::from_uris([
        "mailto:alice@example.org",
        "urn:uuid:5f3c1c0e-0000-4000-8000-000000000001",
        "MAILTO:Alice.Smith@Example.org",
        "https://dav.example.org/principals/alice/",
    ]);
    assert_eq!(
        set,
        CalendarUserAddresses::Known(vec![
            "alice@example.org".to_owned(),
            "Alice.Smith@Example.org".to_owned(),
        ])
    );
}

#[test]
fn a_set_of_only_non_email_identifiers_is_known_and_empty() {
    let set = CalendarUserAddresses::from_uris(["urn:uuid:1", "/principals/alice/"]);
    assert_eq!(set, CalendarUserAddresses::Known(Vec::new()));
    assert!(!set.recognises("alice@example.org"));
}

#[test]
fn a_percent_encoded_address_and_header_fields_are_undone() {
    let set = CalendarUserAddresses::from_uris(["mailto:alice%40example.org?subject=x"]);
    assert_eq!(set.addresses(), ["alice@example.org"]);
}

#[test]
fn a_bare_or_malformed_mailto_is_dropped() {
    let set = CalendarUserAddresses::from_uris(["mailto:", "mailto:alice%4", "mailto:%FF"]);
    assert_eq!(set, CalendarUserAddresses::Known(Vec::new()));
}

#[test]
fn recognition_ignores_case_and_the_mailto_scheme() {
    let set = CalendarUserAddresses::from_uris(["mailto:alice@example.org"]);
    assert!(set.recognises("Alice@EXAMPLE.org"));
    assert!(set.recognises("mailto:alice@example.org"));
    assert!(!set.recognises("bob@example.org"));
}

#[test]
fn neither_not_enabled_nor_unknown_recognises_anything() {
    for set in [
        CalendarUserAddresses::NotEnabled,
        CalendarUserAddresses::Unknown,
    ] {
        assert_eq!(set.addresses(), [] as [String; 0]);
        assert!(!set.recognises("alice@example.org"));
    }
}

#[test]
fn only_a_whole_set_or_none_at_all_rules_an_address_out() {
    let whole = CalendarUserAddresses::Known(vec!["alice@example.org".to_owned()]);
    assert!(!whole.excludes("Alice@example.org"));
    assert!(whole.excludes("info@example.org"));
    assert!(CalendarUserAddresses::NotEnabled.excludes("alice@example.org"));
    assert!(!CalendarUserAddresses::Unknown.excludes("alice@example.org"));
}

#[test]
fn a_partial_set_recognises_what_it_names_and_rules_nothing_out() {
    let partial = CalendarUserAddresses::Including(vec!["alice@example.org".to_owned()]);
    assert_eq!(partial.addresses(), ["alice@example.org"]);
    assert!(partial.recognises("alice@example.org"));
    assert!(!partial.recognises("info@example.org"));
    assert!(
        !partial.excludes("info@example.org"),
        "an alias it does not list"
    );
}

/// The bound-store rule (`calendar_user`'s module docs): in a store opened for another
/// principal, the calendar user is that store's owner, never the credential.
mod bound_store {
    use super::*;

    const OWNER: &str = "support@example.org";
    const USER: &str = "alice@example.org";

    fn known(addresses: &[&str]) -> CalendarUserAddresses {
        CalendarUserAddresses::Known(addresses.iter().map(|&a| a.to_owned()).collect())
    }

    #[test]
    fn an_answer_naming_the_owner_alone_is_for_the_store() {
        assert!(known(&[OWNER]).is_for_store(OWNER, USER));
        assert!(CalendarUserAddresses::Including(vec![OWNER.to_owned()]).is_for_store(OWNER, USER));
        // Case-insensitively, as addresses compare.
        assert!(known(&["Support@Example.org"]).is_for_store(OWNER, USER));
        // An owner's aliases are the owner's too: a principal's address set may list several.
        assert!(known(&[OWNER, "helpdesk@example.org"]).is_for_store(OWNER, USER));
    }

    #[test]
    fn an_answer_that_schedules_as_nobody_is_honest_about_the_store() {
        // A share the user may read but not schedule in: no address is theirs to use.
        for set in [
            CalendarUserAddresses::NotEnabled,
            CalendarUserAddresses::Unknown,
            known(&[]),
        ] {
            assert!(set.is_for_store(OWNER, USER), "{set:?}");
        }
    }

    #[test]
    fn an_answer_that_names_the_credential_speaks_for_the_wrong_principal() {
        // What JMAP's `ParticipantIdentity/get` returns on any account: the user's own
        // identities, the group's among them.
        assert!(!known(&[USER, OWNER]).is_for_store(OWNER, USER));
        assert!(!known(&[USER]).is_for_store(OWNER, USER));
        assert!(!CalendarUserAddresses::Including(vec![USER.to_owned()]).is_for_store(OWNER, USER));
    }

    #[test]
    fn an_answer_naming_someone_else_entirely_is_not_for_the_store() {
        assert!(!known(&["carol@example.org"]).is_for_store(OWNER, USER));
    }

    #[test]
    fn in_the_users_own_store_the_owner_is_the_user() {
        assert!(known(&[USER]).is_for_store(USER, USER));
        // The user's own answer may name more of their addresses (a group they schedule as).
        assert!(known(&[USER, OWNER]).is_for_store(USER, USER));
        assert!(CalendarUserAddresses::NotEnabled.is_for_store(USER, USER));
    }

    #[test]
    fn an_own_store_answer_that_misses_the_user_is_not_for_the_store() {
        assert!(!known(&["carol@example.org"]).is_for_store(USER, USER));
    }
}
