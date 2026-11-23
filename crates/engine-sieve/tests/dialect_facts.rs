//! What the reader learns of a command it carries whole, against the review's probes: `${`
//! across scripts, what may send mail, unknown tokens, the context of a tag, and fallbacks.

use std::collections::BTreeSet;

use engine_core::rules::{
    Expansion, ForwardingRules, Kept, MailRule, PlanMode, RawRule, RuleBody, plan_rules,
};
use engine_sieve::dialect::{DialectErrorKind, carried, read_rules, vocabulary, write_rules};

fn only(script: &str) -> MailRule {
    let mut rules = read_rules(script);
    assert_eq!(rules.len(), 1, "{script:?}");
    rules.remove(0)
}

fn raw(rule: &MailRule) -> &RawRule {
    match &rule.body {
        RuleBody::Opaque(raw) => raw,
        _ => panic!("typed"),
    }
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn an_encoded_character_rule_brings_what_makes_it_mean_the_same() {
    // N1: `${hex:41}` is "A" in its source; carried without `encoded-character` it would match
    // the literal text.
    let source = "require [\"encoded-character\",\"reject\"];\r\nif header :contains \"subject\" \"${hex:41}\" { reject \"no\"; }\r\n";
    let rule = only(source);
    assert_eq!(raw(&rule).expansion(), Some(Expansion::Expanded));
    assert_eq!(
        raw(&rule).requires(),
        Some(&names(&["encoded-character", "reject"]))
    );
    // Over a script that reads `${` literally, it is refused, not written to mean something else.
    let err = write_rules("keep;\r\n", std::slice::from_ref(&rule)).unwrap_err();
    assert_eq!(err.kind(), DialectErrorKind::Variable { rule: 0 });
    // And an import does not carry it to another script at all.
    let plan = plan_rules(
        &[rule],
        &vocabulary(["encoded-character", "reject"], None),
        PlanMode::Import {
            forwarding: ForwardingRules::Enabled,
        },
    );
    assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
}

#[test]
fn a_literal_rule_is_not_written_over_a_script_that_expands() {
    // N2: literal in its source; over a base requiring `variables` it would expand.
    let rule = only(
        "if header :contains \"subject\" \"${x}\" { discard; }\r\nkeep;\r\n"
            .split("keep")
            .next()
            .unwrap(),
    );
    assert_eq!(raw_or_typed_expansion(&rule), Some(Expansion::Literal));
    let base = "require \"variables\";\r\nset \"x\" \"y\";\r\n";
    let mut rules = read_rules(base);
    rules.push(rule);
    let err = write_rules(base, &rules).unwrap_err();
    assert_eq!(err.kind(), DialectErrorKind::Variable { rule: 1 });
}

fn raw_or_typed_expansion(rule: &MailRule) -> Option<Expansion> {
    match &rule.body {
        RuleBody::Opaque(raw) => raw.expansion(),
        // A typed rule with `${` is checked as it is written.
        _ => Some(Expansion::Literal),
    }
}

#[test]
fn a_rule_using_a_variable_set_elsewhere_stays_in_its_script() {
    // N2b: the `set` is another segment; the rule cannot move alone.
    let source = "require \"variables\";\r\nset \"who\" \"boss\";\r\nif header :contains \"from\" \"${who}\" { keep; }\r\n";
    let rules = read_rules(source);
    assert_eq!(raw(&rules[1]).expansion(), Some(Expansion::Expanded));
    let plan = plan_rules(
        &rules[1..],
        &vocabulary(["variables"], None),
        PlanMode::Import {
            forwarding: ForwardingRules::Enabled,
        },
    );
    assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
    // Edited in place, over its own script, it is copied as it was.
    assert_eq!(write_rules(source, &rules).unwrap(), source);
}

#[test]
fn only_what_is_known_to_send_nothing_sends_nothing() {
    for (script, sends) in [
        ("keep;", false),
        ("require \"vacation\";\r\nvacation \"away\";", false),
        ("require \"reject\";\r\nreject \"no\";", false),
        ("redirect \"a@example.com\";", true),
        (
            "require \"enotify\";\r\nnotify \"mailto:a@example.com\";",
            true,
        ),
        ("require \"include\";\r\ninclude \"other\";", true),
        ("require \"vnd.dovecot.pipe\";\r\npipe \"x\";", true),
        ("require \"vnd.dovecot.execute\";\r\nexecute \"x\";", true),
        ("vnd_dovecot_report \"x\";", true),
    ] {
        let rule = only(script);
        assert_eq!(raw(&rule).sends_mail(), sends, "{script}");
    }
    // N3: imported with forwarding off, a pipe does not arrive running.
    let rule = only("require \"vnd.dovecot.pipe\";\r\npipe \"x\";");
    let plan = plan_rules(
        &[rule],
        &vocabulary(["vnd.dovecot.pipe"], None),
        PlanMode::Import {
            forwarding: ForwardingRules::Disabled,
        },
    );
    assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
}

#[test]
fn a_host_written_command_with_an_unknown_token_has_unknown_needs_and_may_send() {
    let unknown = carried("pipe \"x\";").unwrap();
    assert_eq!(unknown.requires(), None);
    assert!(unknown.sends_mail());
    let known = carried("discard;").unwrap();
    assert_eq!(known.requires(), Some(&BTreeSet::new()));
    assert!(!known.sends_mail());
}

#[test]
fn a_tag_needs_what_its_command_makes_it_need() {
    for (script, needs) in [
        (
            "require [\"spamtestplus\",\"relational\"];\r\nif spamtest :percent :value \"gt\" \"50\" { discard; }",
            &["relational", "spamtest", "spamtestplus"][..],
        ),
        (
            "require [\"vacation\",\"vacation-seconds\"];\r\nvacation :seconds 60 \"away\";",
            &["vacation", "vacation-seconds"],
        ),
        (
            "require [\"vacation\"];\r\nvacation :mime \"Content-Type: text/plain\r\n\r\naway\";",
            &["vacation"],
        ),
        (
            "require [\"duplicate\"];\r\nif duplicate :seconds 60 :last { discard; }",
            &["duplicate"],
        ),
        (
            "require [\"editheader\"];\r\naddheader :last \"X-A\" \"b\";",
            &["editheader"],
        ),
        (
            "require [\"mime\"];\r\nif header :mime :type \"Content-Type\" \"text\" { keep; }",
            &["mime"],
        ),
        (
            "require [\"index\",\"date\"];\r\nif date :index 1 :last :zone \"+0000\" \"date\" \"year\" \"2030\" { keep; }",
            &["date", "index"],
        ),
        (
            "require [\"extlists\"];\r\nif header :list \"from\" \":addrbook:default\" { keep; }",
            &["extlists"],
        ),
    ] {
        assert_eq!(
            raw(&only(script)).requires(),
            Some(&names(needs)),
            "{script}"
        );
    }
}

#[test]
fn a_special_use_fallback_is_read_typed_only_as_the_writer_writes_it() {
    let typed = "if true { fileinto :specialuse \"\\\\Junk\" \"Junk\"; }";
    assert!(matches!(only(typed).body, RuleBody::Typed(_)));
    for other in [
        "if true { fileinto :specialuse \"\\\\junk\" \"junk\"; }",
        "if true { fileinto :specialuse \"Junk\" \"unk\"; }",
        "if true { fileinto :specialuse \"\\\\Junk\" \"junk\"; }",
    ] {
        assert!(matches!(only(other).body, RuleBody::Opaque(_)), "{other}");
    }
}
