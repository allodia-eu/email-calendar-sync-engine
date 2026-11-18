use super::Importance;

#[test]
fn absent_headers_are_normal() {
    assert_eq!(Importance::from_headers(None, None), Importance::Normal);
    assert_eq!(Importance::default(), Importance::Normal);
}

#[test]
fn importance_is_read_without_regard_to_case_or_folding() {
    for (value, expected) in [
        ("high", Importance::High),
        (" High\r\n", Importance::High),
        ("LOW", Importance::Low),
        ("Normal", Importance::Normal),
    ] {
        assert_eq!(
            Importance::from_headers(Some(value), None),
            expected,
            "{value:?}"
        );
    }
}

#[test]
fn x_priority_is_read_by_its_leading_digit() {
    for (value, expected) in [
        ("1", Some(Importance::High)),
        ("1 (Highest)", Some(Importance::High)),
        ("2 (High)", Some(Importance::High)),
        (" 3 (Normal)", Some(Importance::Normal)),
        ("4 (Low)", Some(Importance::Low)),
        ("5", Some(Importance::Low)),
        ("0", None),
        ("6", None),
        ("10", None),
        ("urgent", None),
        ("", None),
    ] {
        assert_eq!(Importance::from_x_priority(value), expected, "{value:?}");
    }
}

#[test]
fn importance_wins_over_x_priority() {
    assert_eq!(
        Importance::from_headers(Some("low"), Some("1 (Highest)")),
        Importance::Low
    );
}

#[test]
fn x_priority_stands_in_for_an_unreadable_importance() {
    assert_eq!(Importance::from_headers(None, Some("1")), Importance::High);
    assert_eq!(
        Importance::from_headers(Some("very"), Some("5")),
        Importance::Low
    );
    assert_eq!(
        Importance::from_headers(Some("very"), Some("x")),
        Importance::Normal
    );
}

#[test]
fn outgoing_headers_round_trip() {
    for importance in [Importance::Low, Importance::Normal, Importance::High] {
        assert_eq!(Importance::parse(importance.as_str()), Some(importance));
        let back = importance.x_priority().map_or(Importance::Normal, |value| {
            Importance::from_x_priority(value).unwrap()
        });
        assert_eq!(back, importance);
    }
    assert!(Importance::Normal.is_normal());
    assert_eq!(Importance::Normal.x_priority(), None);
}

#[test]
fn serializes_as_the_rfc_token() {
    assert_eq!(
        serde_json::to_string(&Importance::High).unwrap(),
        "\"high\""
    );
    assert_eq!(
        serde_json::from_str::<Importance>("\"low\"").unwrap(),
        Importance::Low
    );
}
