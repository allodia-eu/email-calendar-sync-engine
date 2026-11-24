//! A script read for a transport whose folder ids are not RFC 8474 `MAILBOXID`s: a rule filing by
//! id is carried whole, as its exact text, and written back byte for byte.

use engine_core::rules::{Action, RuleBody, RuleMailbox, RuleName};
use engine_sieve::dialect::{folder_ids_carried, read_rules, write_rules};

const SCRIPT: &str = "require [\"fileinto\",\"mailboxid\"];\r\n# rule:[By id]\r\nif header \
                      :contains \"subject\" \"a\"\r\n{\r\n\tfileinto :mailboxid \"Fa\" \
                      \"INBOX\";\r\n}\r\n# rule:[Off]\r\nif false # header :contains \"subject\" \
                      \"b\"\r\n{\r\n\tfileinto :mailboxid \"Fb\" \"INBOX\";\r\n}\r\n# \
                      rule:[By name]\r\nif header :contains \"subject\" \"c\"\r\n{\r\n\tfileinto \
                      \"Archive\";\r\n}\r\n";

#[test]
fn a_rule_filing_by_id_is_carried_whole_and_written_back_as_it_was() {
    let typed = read_rules(SCRIPT);
    assert!(
        matches!(&typed[0].body, RuleBody::Typed(rule)
            if matches!(rule.actions[0], Action::FileInto { mailbox: RuleMailbox::Id(_), .. })),
        "read typed where ids are folder ids"
    );
    let rules = folder_ids_carried(SCRIPT, typed.clone());
    assert_eq!(rules.len(), 3);
    for (at, name) in [(0, "By id"), (1, "Off")] {
        let RuleBody::Opaque(raw) = &rules[at].body else {
            panic!("rule {at} is carried whole");
        };
        assert!(raw.text().contains(":mailboxid"), "{at}");
        assert_eq!(raw.name().map(RuleName::as_str), Some(name));
        assert_eq!(rules[at].name.as_ref().map(RuleName::as_str), Some(name));
        assert!(rules[at].enabled, "its text decides whether it runs");
        assert_eq!(rules[at].origin, typed[at].origin);
        assert!(
            raw.requires()
                .is_some_and(|needs| needs.contains("mailboxid")),
            "{at}"
        );
    }
    assert_eq!(rules[2], typed[2], "a rule filing by name stays typed");
    assert_eq!(write_rules(SCRIPT, &rules).unwrap(), SCRIPT);
}

#[test]
fn rules_from_another_script_are_left_as_they_are() {
    let typed = read_rules(SCRIPT);
    let other = format!("{SCRIPT}keep;\r\n");
    assert_eq!(folder_ids_carried(&other, typed.clone()), typed);
}
