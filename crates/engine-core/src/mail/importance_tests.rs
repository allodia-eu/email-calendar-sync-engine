use super::{Importance, ImportanceHeaders};

fn importance(value: &str) -> ImportanceHeaders<'_> {
    ImportanceHeaders {
        importance: Some(value),
        ..ImportanceHeaders::default()
    }
}

fn x_priority(value: &str) -> ImportanceHeaders<'_> {
    ImportanceHeaders {
        x_priority: Some(value),
        ..ImportanceHeaders::default()
    }
}

#[test]
fn a_message_stating_nothing_is_normal() {
    assert_eq!(
        Importance::from_headers(ImportanceHeaders::default()),
        Importance::Normal
    );
    assert_eq!(Importance::default(), Importance::Normal);
}

#[test]
fn importance_is_read_whatever_its_case_whitespace_and_comments() {
    for (value, expected) in [
        ("high", Some(Importance::High)),
        ("HIGH", Some(Importance::High)),
        (" High\r\n", Some(Importance::High)),
        ("low  ", Some(Importance::Low)),
        ("\tlow", Some(Importance::Low)),
        ("High (urgent)", Some(Importance::High)),
        ("(sender's choice) low", Some(Importance::Low)),
        ("high (a (nested) comment)", Some(Importance::High)),
        ("high (a \\) quoted paren)", Some(Importance::High)),
        ("Normal", Some(Importance::Normal)),
        ("very", None),
        ("high low", None),
        ("", None),
        ("(only a comment)", None),
    ] {
        assert_eq!(Importance::parse(value), expected, "{value:?}");
    }
}

#[test]
fn x_priority_is_one_digit_whatever_comment_follows() {
    for (value, expected) in [
        ("1", Some(Importance::High)),
        ("1 (Highest)", Some(Importance::High)),
        ("2 (High)", Some(Importance::High)),
        (" 3 (Normal)", Some(Importance::Normal)),
        ("4 (Low)", Some(Importance::Low)),
        ("5", Some(Importance::Low)),
        ("5(Lowest)", Some(Importance::Low)),
        ("0", None),
        ("6", None),
        ("10", None),
        ("1.0", None),
        ("High", None),
        ("urgent", None),
        ("", None),
    ] {
        assert_eq!(Importance::from_x_priority(value), expected, "{value:?}");
    }
}

#[test]
fn the_first_header_present_decides() {
    let all = |i, m, x| ImportanceHeaders {
        importance: i,
        x_msmail_priority: m,
        x_priority: x,
    };
    // Importance over both others; X-MSMail-Priority over X-Priority.
    assert_eq!(
        Importance::from_headers(all(Some("low"), Some("High"), Some("1"))),
        Importance::Low
    );
    assert_eq!(
        Importance::from_headers(all(None, Some("Low"), Some("1"))),
        Importance::Low
    );
    assert_eq!(
        Importance::from_headers(all(None, None, Some("1"))),
        Importance::High
    );
}

#[test]
fn a_header_present_but_unreadable_states_normal_rather_than_falling_back() {
    let headers = ImportanceHeaders {
        importance: Some("very"),
        x_msmail_priority: Some("High"),
        x_priority: Some("1"),
    };
    assert_eq!(Importance::from_headers(headers), Importance::Normal);
    let headers = ImportanceHeaders {
        x_msmail_priority: Some("urgent"),
        x_priority: Some("1"),
        ..ImportanceHeaders::default()
    };
    assert_eq!(Importance::from_headers(headers), Importance::Normal);
    assert_eq!(Importance::from_headers(importance("")), Importance::Normal);
    assert_eq!(
        Importance::from_headers(x_priority("High")),
        Importance::Normal
    );
}

#[test]
fn a_lookup_is_asked_for_every_header_by_its_name() {
    let mut asked = Vec::new();
    let headers = ImportanceHeaders::from_lookup(|name| {
        asked.push(name.to_owned());
        (name == "X-Priority").then_some("5")
    });
    assert_eq!(asked, Importance::HEADERS);
    assert_eq!(headers, x_priority("5"));
    assert_eq!(Importance::from_headers(headers), Importance::Low);
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
