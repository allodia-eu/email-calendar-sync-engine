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
        assert!(set.addresses().is_empty());
        assert!(!set.recognises("alice@example.org"));
    }
}
