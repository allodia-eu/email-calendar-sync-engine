use super::*;

fn v(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn a_field_holds_one_value_or_an_any_of_in_each_spelling() {
    assert_eq!(
        field_values("alice@example.com"),
        Some(v(&["alice@example.com"]))
    );
    assert_eq!(field_values("  invoice  "), Some(v(&["invoice"])));
    assert_eq!(
        field_values("\"weekly report\""),
        Some(v(&["weekly report"]))
    );
    for any in [
        "a@example.com OR b@example.com",
        "(a@example.com OR b@example.com)",
        "{a@example.com b@example.com}",
    ] {
        assert_eq!(
            field_values(any),
            Some(v(&["a@example.com", "b@example.com"])),
            "{any}"
        );
    }
    assert_eq!(
        field_values("x+tag@example.com"),
        Some(v(&["x+tag@example.com"]))
    );
}

/// Whatever the subset does not read stays whole: a lost part would change what the filter
/// matches. `me` is the account itself, bare or quoted, until a quoted one is proven a word.
#[test]
fn anything_outside_the_subset_is_not_a_value() {
    for text in [
        "",
        "two words",
        "a or b",
        "-alice",
        "+alice",
        "me",
        "ME",
        "\"me\"",
        "\"Me\"",
        "OR",
        "a OR",
        "\"open",
        "\"\"",
        "\"  \"",
        "a:b",
        "( a OR b )",
        "(a AND b)",
        "{a}b",
        "a|b",
    ] {
        assert_eq!(field_values(text), None, "{text:?}");
    }
}

#[test]
fn a_conjunction_reads_every_term_of_the_subset() {
    let terms = conjunction(
        "from:(a@example.com OR b@example.com) subject:\"status update\" \
         has:attachment larger:1000000 smaller:20000000",
    );
    assert_eq!(
        terms,
        Some(vec![
            Term::From(v(&["a@example.com", "b@example.com"])),
            Term::Subject(v(&["status update"])),
            Term::HasAttachment,
            Term::Larger(1_000_000),
            Term::Smaller(20_000_000),
        ])
    );
}

#[test]
fn a_search_with_any_other_term_or_operator_is_not_read() {
    for text in [
        "invoice",
        "from:a@example.com invoice",
        "from:a@example.com OR subject:x",
        "-from:a@example.com",
        "has:attachments",
        "has:drive",
        "larger:10M",
        "larger:",
        "larger:-1",
        "from:me",
        "from:\"me\"",
        "to:a@example.com",
        "cc:a@example.com",
        "in:inbox",
        "from:a@example.com(subject:x)",
        "from:\"a\"subject:x",
        "category:promotions",
        "list:list.example.com",
        "subject:(a b)",
    ] {
        assert_eq!(conjunction(text), None, "{text:?}");
    }
}

#[test]
fn a_disjunction_is_terms_joined_by_or() {
    assert_eq!(
        disjunction("from:a@example.com OR subject:x"),
        Some(vec![
            Term::From(v(&["a@example.com"])),
            Term::Subject(v(&["x"]))
        ])
    );
    assert_eq!(
        disjunction("has:attachment"),
        Some(vec![Term::HasAttachment])
    );
    // Terms that must all hold are not a disjunction: NOT (a AND b) is no list of exceptions.
    assert_eq!(disjunction("from:a@example.com subject:x"), None);
    assert_eq!(disjunction("from:a@example.com or subject:x"), None);
}

#[test]
fn a_term_is_written_as_it_is_read_back() {
    let terms = [
        Term::From(v(&["alice@example.com"])),
        Term::From(v(&["a@example.com", "b c"])),
        Term::Subject(v(&["quarterly report", "OR", "-draft"])),
        Term::HasAttachment,
        Term::Larger(5),
        Term::Smaller(0),
    ];
    for term in terms {
        let text = term.render().expect("renders");
        assert_eq!(conjunction(&text), Some(vec![term.clone()]), "{text}");
        assert_eq!(disjunction(&text), Some(vec![term]), "{text}");
    }
    assert_eq!(
        Term::Subject(v(&["a", "b"])).render().as_deref(),
        Some("subject:(a OR b)")
    );
    for unwritable in ["say \"hi\"", "me", "Me", "line\nbreak", "  "] {
        assert_eq!(
            Term::From(v(&[unwritable])).render(),
            None,
            "{unwritable:?}"
        );
    }
}

#[test]
fn field_values_are_written_as_they_are_read_back() {
    for values in [
        v(&["alice@example.com"]),
        v(&["a b", "c"]),
        v(&["OR", "AND"]),
    ] {
        let text = render_values(&values).expect("renders");
        assert_eq!(field_values(&text), Some(values), "{text}");
    }
    assert_eq!(render_values(&v(&["line\nbreak"])), None);
    assert_eq!(render_values(&v(&["   "])), None);
    assert_eq!(render_values(&v(&["me"])), None);
}

fn test(op: TextOp, values: &[&str]) -> TextTest {
    TextTest::new(op, v(values)).unwrap()
}

#[test]
fn terms_and_conditions_correspond() {
    for term in [
        Term::From(v(&["a"])),
        Term::Subject(v(&["c"])),
        Term::HasAttachment,
        Term::Larger(1),
        Term::Smaller(2),
    ] {
        let condition = term.clone().condition().expect("typed");
        assert_eq!(Term::of(&condition), Some(term));
    }
    // A subject that contains a value is written as a word search, which matches less.
    let contains = Condition::Header {
        field: HeaderField::Subject,
        test: test(TextOp::Contains, &["a"]),
    };
    assert_eq!(Term::of(&contains), Some(Term::Subject(v(&["a"]))));
    for not_held in [
        // A word search matches the display name too: an address alone has no spelling.
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::All,
            test: test(TextOp::Contains, &["a"]),
        },
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::All,
            test: test(TextOp::Is, &["a@example.com"]),
        },
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::WithName,
            test: test(TextOp::Contains, &["a"]),
        },
        Condition::Address {
            field: AddressField::ToOrCc,
            part: AddressPart::WithName,
            test: test(TextOp::Words, &["a"]),
        },
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::Domain,
            test: test(TextOp::Is, &["example.com"]),
        },
        Condition::Header {
            field: HeaderField::Subject,
            test: test(TextOp::Regex, &["a.*"]),
        },
        // A whole subject has no spelling: Gmail would match it within a longer one.
        Condition::Header {
            field: HeaderField::Subject,
            test: test(TextOp::Is, &["a"]),
        },
        Condition::Header {
            field: HeaderField::named("X-Tag"),
            test: test(TextOp::Words, &["a"]),
        },
        Condition::Body(test(TextOp::Words, &["a"])),
    ] {
        assert_eq!(Term::of(&not_held), None, "{not_held:?}");
    }
}
