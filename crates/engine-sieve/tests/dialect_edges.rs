//! The edges of the dialect: what reads typed though Roundcube would not write it so, everything
//! it does not write (carried whole, never misread), and the writer's corners.

use engine_core::{
    mail::MailboxRole,
    rules::{
        Action, Condition, HeaderField, MailRule, MailboxPath, Match, MatchMode, RuleAddress,
        RuleBody, RuleMailbox, SizeBound, TextOp, TextTest,
    },
};
use engine_sieve::dialect::{DialectErrorKind, read_rules, vocabulary, write_rules};

fn only(script: &str) -> MailRule {
    let rules = read_rules(script);
    assert_eq!(rules.len(), 1, "{script:?}");
    let mut rule = rules.into_iter().next().unwrap();
    rule.origin = None;
    rule
}

fn typed(script: &str) -> (Match, Vec<Action>, bool) {
    match only(script).body {
        RuleBody::Typed(rule) => (rule.when, rule.actions, rule.stop),
        RuleBody::Opaque(_) => panic!("read whole: {script:?}"),
        _ => unreachable!(),
    }
}

fn subject(op: TextOp, value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(op, vec![value.to_owned()]).unwrap(),
    }
}

#[test]
fn what_the_dialect_does_not_write_is_carried_whole_never_misread() {
    for script in [
        "if true { stop; discard; }",
        "if true { 42; }",
        "if true { discard }",
        "if true discard;",
        "if true { addheader \"X-Other\" \"v\"; }",
        "if true { addheader \"Importance\" \"urgent\"; }",
        "if true { fileinto :create \"x\"; }",
        "if true { fileinto :specialuse \"\\\\Inbox\" \"x\"; }",
        "if true { fileinto :specialuse \"\\\\Trash\" :mailboxid \"m\" \"x\"; }",
        "if true { fileinto :mailboxid \"\" \"x\"; }",
        "if true { redirect :other \"a@example.com\"; }",
        "if true { addflag \"\\\\Deleted\"; }",
        "if true { keep; }",
        "if true { discard; } else { keep; }",
        "if 42 { discard; }",
        "if exists \"x\" { discard; }",
        "if not not true { discard; }",
        "if anyof (not true, true) { discard; }",
        "if allof (true, header :is \"a\" \"b\") { discard; }",
        "if allof (header :is \"a\" \"b\", anyof (true)) { discard; }",
        "if allof (anyof (header :is \"a\" \"b\"), header :is \"c\" \"d\") { discard; }",
        "if header :comparator \"i;octet\" :is \"a\" \"b\" { discard; }",
        "if header :is :contains \"a\" \"b\" { discard; }",
        "if header :value \"eq\" \"a\" \"b\" { discard; }",
        "if header :mime :anychild :contains \"Content-Type\" \"x\" { discard; }",
        "if header :is [\"a\",\"b\"] \"c\" { discard; }",
        "if header :is \"a\" [\"b\",] { discard; }",
        "if address :all :domain \"from\" \"x\" { discard; }",
        "if address :user \"from\" \"x\" { discard; }",
        "if address \"bcc\" \"x\" { discard; }",
        "if size :between 1 { discard; }",
        "if size :over \"1\" { discard; }",
        "if size :over 99999999999999999999G { discard; }",
        "if size :over 18014398509481984G { discard; }",
        "if body :raw :contains \"x\" { discard; }",
        "if false # true junk\r\n{ discard; }",
        "if false # 42\r\n{ discard; }",
        "if false # \"unclosed\r\n{ discard; }",
        "if false\r\n{ discard; }",
        "keep;",
    ] {
        let rule = only(script);
        assert!(matches!(rule.body, RuleBody::Opaque(_)), "{script:?}");
        // And it is written back as it was.
        assert_eq!(write_rules(script, &read_rules(script)).unwrap(), script);
    }
}

#[test]
fn forms_roundcube_does_not_write_but_mean_the_same_read_typed() {
    let (when, actions, _) = typed(
        "if address :all :comparator \"i;ascii-casemap\" :contains \"From\" \"x\" \
         { redirect \"a@example.com\"; redirect \"b@example.com\"; }",
    );
    assert_eq!(when.conditions.len(), 1);
    assert_eq!(
        actions,
        [Action::Redirect {
            to: vec![
                RuleAddress::new("a@example.com").unwrap(),
                RuleAddress::new("b@example.com").unwrap()
            ],
            copy: false,
        }]
    );
    let (when, ..) = typed("if anyof (header :is \"subject\" \"a\") { discard; }");
    assert_eq!(when, Match::all(vec![subject(TextOp::Is, "a")]));
    let (when, ..) = typed(
        "if allof (anyof (header :is \"subject\" \"a\", header :is \"subject\" \"b\"), \
         not size :over 1) { discard; }",
    );
    assert_eq!(when.mode, MatchMode::Any);
    assert_eq!(when.except, [Condition::Size(SizeBound::Over(1))]);
    let (when, ..) = typed("if not header :is \"subject\" \"a\" { discard; }");
    assert_eq!(
        when,
        Match::all(Vec::new()).except(vec![subject(TextOp::Is, "a")])
    );
    let (when, ..) = typed("if body :contains \"x\" { discard; }");
    assert_eq!(
        when.conditions,
        [Condition::Body(
            TextTest::new(TextOp::Contains, vec!["x".to_owned()]).unwrap()
        )]
    );
    let (when, ..) = typed("if size :under 1k { discard; }");
    assert_eq!(when.conditions, [Condition::Size(SizeBound::Under(1024))]);
    let (when, ..) = typed("if size :over 2G { discard; }");
    assert_eq!(when.conditions, [Condition::Size(SizeBound::Over(2 << 30))]);
    let (_, actions, _) = typed("if true { addflag \"a\"; addflag \"b\"; }");
    assert_eq!(actions.len(), 1);
    let (_, actions, stop) = typed("if true { fileinto \"inbox\"; stop; }");
    assert!(stop);
    assert!(matches!(
        &actions[0],
        Action::FileInto { mailbox: RuleMailbox::Role(role), .. } if *role == MailboxRole::Inbox
    ));
}

#[test]
fn exceptions_alone_and_with_any_are_written_so_they_read_back() {
    for when in [
        Match::all(Vec::new()).except(vec![subject(TextOp::Is, "a"), subject(TextOp::Is, "b")]),
        Match::any(vec![subject(TextOp::Is, "a"), subject(TextOp::Is, "b")])
            .except(vec![Condition::Size(SizeBound::Under(10))]),
        Match::any(vec![subject(TextOp::Is, "a")]).except(vec![subject(TextOp::Is, "b")]),
    ] {
        let rule = MailRule::typed(when.clone(), vec![Action::Discard]);
        let script = write_rules("", &[rule]).unwrap();
        let (read, ..) = typed(&script);
        let expected = if when.conditions.len() == 1 {
            Match::all(when.conditions.clone()).except(when.except.clone())
        } else {
            when
        };
        assert_eq!(read, expected, "{script:?}");
    }
}

#[test]
fn a_rule_needing_an_extension_over_a_script_with_no_require_gets_one_first() {
    let mut rules = read_rules("keep;\r\n");
    rules.push(MailRule::typed(
        Match::default(),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Path(MailboxPath::new("x").unwrap()),
            copy: false,
        }],
    ));
    assert_eq!(
        write_rules("keep;\r\n", &rules).unwrap(),
        "require [\"fileinto\"];\r\nkeep;\r\nif true\r\n{\r\n\tfileinto \"x\";\r\n}\r\n"
    );
}

#[test]
fn a_require_that_cannot_be_written_again_refuses_the_write() {
    // A capability read from a `text:` block with LF line ends holds a bare LF.
    let base = "require text:\nfoo\n.\n;\r\nkeep;\r\n";
    let mut rules = read_rules(base);
    rules.push(MailRule::typed(
        Match::default(),
        vec![Action::AddKeywords(
            [engine_core::mail::Keyword::new("x").unwrap()].into(),
        )],
    ));
    let err = write_rules(base, &rules).unwrap_err();
    assert!(matches!(err.kind(), DialectErrorKind::Require(_)));
    assert!(err.to_string().contains("require"));
}

#[test]
fn a_vocabulary_without_listed_extensions_or_limits_holds_base_sieve_alone() {
    use engine_core::rules::RuleFeature;
    let base = vocabulary(["copy"], None);
    assert_eq!(base.max_redirects(), None);
    assert!(
        base.support(RuleFeature::FileInto).is_none(),
        "copy without fileinto files nothing"
    );
    assert!(base.support(RuleFeature::FileIntoCopy).is_none());
    assert!(base.support(RuleFeature::RedirectCopy).is_some());
    assert!(base.support(RuleFeature::Body(TextOp::Is)).is_none());
    assert!(
        base.support(RuleFeature::SubjectText(TextOp::Regex))
            .is_none()
    );
}

/// A word match (a search's, Gmail's) has no Sieve test: the vocabulary does not hold it, so the
/// plan drops a rule with one wherever the dialect is written, and the writer refuses it too.
#[test]
fn a_word_match_is_held_by_no_sieve_vocabulary_and_written_by_no_script() {
    use engine_core::rules::{AddressField, AddressPart, Kept, PlanMode, plan_rules};
    let words = TextTest::new(TextOp::Words, vec!["invoice".to_owned()]).unwrap();
    let all = vocabulary(["regex", "body", "fileinto", "imap4flags", "copy"], Some(5));
    for condition in [
        Condition::Header {
            field: HeaderField::Subject,
            test: words.clone(),
        },
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::WithName,
            test: TextTest::new(TextOp::Contains, vec!["a".to_owned()]).unwrap(),
        },
    ] {
        let rule = MailRule::typed(Match::all(vec![condition]), vec![Action::Discard]);
        let plan = plan_rules(std::slice::from_ref(&rule), &all, PlanMode::Edit);
        assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
        assert!(write_rules("", &[rule]).is_err());
    }
}
