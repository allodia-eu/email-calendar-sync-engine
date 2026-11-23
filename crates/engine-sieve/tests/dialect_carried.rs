//! What the dialect learns of a rule it carries whole, and the writer's safety corners: the
//! extensions such a rule needs and whether it may send mail, origins tied to their script,
//! marks before deliveries, fallbacks, `$`-keywords, variables, and where a name line goes.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, SystemKeyword},
    rules::{
        Action, Condition, ForwardingRules, HeaderField, Kept, MailRule, MailboxPath, Match,
        PlanMode, RuleBody, RuleMailbox, RuleName, TextOp, TextTest, plan_rules,
    },
};
use engine_sieve::dialect::{DialectErrorKind, carried, read_rules, vocabulary, write_rules};

fn only(script: &str) -> MailRule {
    let mut rules = read_rules(script);
    assert_eq!(rules.len(), 1, "{script:?}");
    rules.remove(0)
}

fn raw(rule: &MailRule) -> &engine_core::rules::RawRule {
    match &rule.body {
        RuleBody::Opaque(raw) => raw,
        _ => panic!("typed"),
    }
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_rule_carried_whole_knows_the_extensions_it_needs_and_whether_it_sends_mail() {
    let rule = only(
        "require [\"envelope\",\"copy\",\"vacation\"];\r\nif envelope :is \"from\" \"x@example.com\" { redirect :copy \"y@example.net\"; }\r\n",
    );
    let raw = raw(&rule);
    assert_eq!(raw.requires(), Some(&names(&["copy", "envelope"])));
    assert!(raw.sends_mail());
    let rule = only("require [\"enotify\"];\r\nif true { notify \"mailto:a@example.com\"; }\r\n");
    assert!(raw_of(&rule).sends_mail(), "enotify can send mail");
    let rule = only("require \"vacation\";\r\nvacation :days 1 \"away\";\r\n");
    assert_eq!(raw_of(&rule).requires(), Some(&names(&["vacation"])));
    assert!(!raw_of(&rule).sends_mail());
    // An identifier the table does not know: everything the script required, so nothing it
    // needs is ever left out.
    let rule = only("require [\"vnd.example.x\",\"fileinto\"];\r\nvnd_example_x;\r\n");
    assert_eq!(
        raw_of(&rule).requires(),
        Some(&names(&["fileinto", "vnd.example.x"]))
    );
    let rule =
        only("if header :comparator \"i;ascii-numeric\" :value \"gt\" \"x\" \"1\" { keep; }");
    assert_eq!(
        raw_of(&rule).requires(),
        Some(&names(&["comparator-i;ascii-numeric", "relational"]))
    );
}

fn raw_of(rule: &MailRule) -> &engine_core::rules::RawRule {
    raw(rule)
}

#[test]
fn an_import_with_forwarding_off_carries_no_text_that_may_send_mail() {
    let rules = read_rules(
        "require \"envelope\";\r\nif envelope :is \"from\" \"x@example.com\" { redirect \"evil@example.net\"; }\r\nkeep;\r\n",
    );
    let target = vocabulary(["envelope"], Some(4));
    let plan = plan_rules(
        &rules,
        &target,
        PlanMode::Import {
            forwarding: ForwardingRules::Disabled,
        },
    );
    assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
    assert_eq!(plan.outcomes[1].kept, Kept::Whole, "keep sends nothing");
}

#[test]
fn a_rule_carried_from_another_script_brings_its_needs_and_not_the_other_s_comments() {
    let source = "require \"vacation\";\r\n# rule:[Away]\r\nvacation \"away\";\r\n";
    let target = "# A header comment\r\nrequire \"fileinto\";\r\nkeep;\r\n";
    let mut rules = read_rules(target);
    rules.extend(read_rules(source));
    let plan = plan_rules(
        &rules,
        &vocabulary(["fileinto", "vacation"], None),
        PlanMode::Edit,
    );
    assert!(
        plan.outcomes
            .iter()
            .all(|outcome| outcome.kept == Kept::Whole)
    );
    let written = write_rules(target, &plan.written()).unwrap();
    assert_eq!(
        written,
        "# A header comment\r\nrequire [\"fileinto\",\"vacation\"];\r\nkeep;\r\n# rule:[Away]\r\nvacation \"away\";\r\n"
    );
    // A target that does not list what it needs does not get it.
    let plan = plan_rules(&rules, &vocabulary(["fileinto"], None), PlanMode::Edit);
    assert_eq!(plan.outcomes[1].kept, Kept::NotKept);
    // A rule read from another script at index 0 does not take this script's segment 0 for its
    // own, nor the comment that leads it.
    let other = read_rules("# rule:[A]\r\nif true\r\n{\r\n\tdiscard;\r\n}\r\n");
    let mut both = read_rules(target);
    both.extend(other);
    let written = write_rules(target, &both).unwrap();
    assert_eq!(
        written.matches("# A header comment").count(),
        1,
        "{written:?}"
    );
    // A typed rule read from another script is rendered, not taken for this one's segment 1.
    let typed = read_rules(
        "require \"fileinto\";\r\n# rule:[A]\r\nif true\r\n{\r\n\tfileinto \"x\";\r\n}\r\n",
    );
    let written = write_rules("# lead\r\nkeep;\r\n", &typed).unwrap();
    assert!(
        written.starts_with("require [\"fileinto\"];\r\n# rule:[A]\r\nif true"),
        "{written:?}"
    );
}

#[test]
fn a_command_written_by_a_host_is_carried_with_its_needs() {
    let include = carried("include :personal :optional \"vacation\";\r\n").unwrap();
    assert_eq!(include.requires(), Some(&names(&["include"])));
    assert!(!include.sends_mail());
    let written = write_rules("", &[MailRule::opaque(include)]).unwrap();
    assert_eq!(
        written,
        "require [\"include\"];\r\ninclude :personal :optional \"vacation\";\r\n"
    );
    assert!(carried("keep; stop;").is_none(), "two commands");
    assert!(carried("# only a comment").is_none());
}

#[test]
fn marks_are_written_before_the_message_is_delivered_and_read_only_in_that_order() {
    let rule = MailRule::typed(
        Match::default(),
        vec![
            Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Board").unwrap()),
                copy: false,
            },
            Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into()),
        ],
    );
    let written = write_rules("", &[rule]).unwrap();
    assert!(
        written.contains("\taddflag \"\\\\Seen\";\r\n\tfileinto \"Board\";"),
        "{written:?}"
    );
    assert!(matches!(only(&written).body, RuleBody::Typed(_)));
    // The other order means something else: the filed copy is not flagged (RFC 5232 §3).
    let later = "if true { fileinto \"Board\"; addflag \"\\\\Seen\"; }";
    assert!(matches!(only(later).body, RuleBody::Opaque(_)));
    let later = "if true { discard; addheader \"Importance\" \"high\"; }";
    assert!(matches!(only(later).body, RuleBody::Opaque(_)));
}

#[test]
fn a_fallback_folder_other_than_the_writer_s_is_carried_whole() {
    let typed = "if true { fileinto :specialuse \"\\\\Junk\" \"Junk\"; }";
    assert!(matches!(only(typed).body, RuleBody::Typed(_)));
    for other in [
        "if true { fileinto :specialuse \"\\\\Junk\" \"Spam\"; }",
        "if true { fileinto :mailboxid \"m1\" \"Archive\"; }",
    ] {
        assert!(matches!(only(other).body, RuleBody::Opaque(_)), "{other}");
    }
    let by_id = "if true { fileinto :mailboxid \"m1\" \"INBOX\"; }";
    assert!(matches!(only(by_id).body, RuleBody::Typed(_)));
}

#[test]
fn a_system_keyword_spelled_with_a_dollar_is_carried_whole() {
    for spelled in ["$seen", "$Flagged", "$answered", "$draft"] {
        let script = format!("if true {{ addflag \"{spelled}\"; }}");
        assert!(
            matches!(only(&script).body, RuleBody::Opaque(_)),
            "{spelled}"
        );
    }
    assert!(matches!(
        only("if true { addflag \"$label1\"; }").body,
        RuleBody::Typed(_)
    ));
}

#[test]
fn a_value_the_script_s_variables_would_expand_is_refused() {
    let base = "require \"variables\";\r\nkeep;\r\n";
    let mut rules = read_rules(base);
    rules.push(MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec!["${x}".to_owned()]).unwrap(),
        }]),
        vec![Action::Discard],
    ));
    let err = write_rules(base, &rules).unwrap_err();
    assert_eq!(err.kind(), DialectErrorKind::Variable { rule: 1 });
    assert!(err.to_string().contains("expand"));
    // Without variables, it is literal text.
    assert!(write_rules("", &rules[1..]).is_ok());
}

#[test]
fn a_renamed_indented_rule_keeps_its_name_at_the_start_of_a_line() {
    let script = "keep;\r\n  if true { discard; }\r\n";
    let mut rules = read_rules(script);
    rules[1].name = Some(RuleName::new("Now named").unwrap());
    let written = write_rules(script, &rules).unwrap();
    assert!(
        written.contains("\r\n# rule:[Now named]\r\n"),
        "{written:?}"
    );
    assert_eq!(
        read_rules(&written)[1].name.as_ref().unwrap().as_str(),
        "Now named"
    );
}

#[test]
fn a_disabled_rule_whose_test_spans_lines_is_written_so_it_reads_back_disabled() {
    let rule = MailRule::typed(
        Match::all(vec![Condition::Body(
            TextTest::new(TextOp::Contains, vec!["two\r\nlines".to_owned()]).unwrap(),
        )]),
        vec![Action::Discard],
    )
    .disabled();
    let written = write_rules("", std::slice::from_ref(&rule)).unwrap();
    assert!(written.contains("if allof (false, body"), "{written:?}");
    let mut back = only(&written);
    back.origin = None;
    assert_eq!(back, rule);
}
