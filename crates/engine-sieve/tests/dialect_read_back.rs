//! A rule as a script the dialect writes holds it, read back; and a rule carried whole, its facts
//! read from its own text rather than from whoever built it.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, MailboxRole},
    rules::{
        Action, Condition, HeaderField, MailRule, MailboxPath, Match, RawRule, RuleAddress,
        RuleBody, RuleMailbox, RuleName, RuleSyntax, TextOp, TextTest,
    },
};
use engine_sieve::dialect::{facts_from_text, read_back};

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

fn address(value: &str) -> RuleAddress {
    RuleAddress::new(value).unwrap()
}

fn rule(when: Match, actions: Vec<Action>) -> MailRule {
    MailRule::typed(when, actions).named(RuleName::new("R").unwrap())
}

fn actions(rule: &MailRule) -> &[Action] {
    match &rule.body {
        RuleBody::Typed(typed) => &typed.actions,
        _ => panic!("read back typed"),
    }
}

#[test]
fn a_forward_reads_back_as_the_redirect_that_keeps_a_copy() {
    let forward = rule(
        Match::all(vec![subject("a")]),
        vec![Action::Forward {
            to: vec![address("away@example.com")],
        }],
    )
    .disabled();
    let back = read_back(&forward).unwrap();
    assert_eq!(
        actions(&back),
        [Action::Redirect {
            to: vec![address("away@example.com")],
            copy: true
        }]
    );
    assert!(!back.enabled, "still disabled");
    assert_eq!(back.origin, None);
}

#[test]
fn two_redirects_alike_and_two_keyword_actions_read_back_as_one() {
    let redirect = |to: &str| Action::Redirect {
        to: vec![address(to)],
        copy: false,
    };
    let back = read_back(&rule(
        Match::all(vec![subject("a")]),
        vec![redirect("one@example.com"), redirect("two@example.com")],
    ))
    .unwrap();
    assert_eq!(
        actions(&back),
        [Action::Redirect {
            to: vec![address("one@example.com"), address("two@example.com")],
            copy: false
        }]
    );
    let keywords = |name: &str| Action::AddKeywords([Keyword::new(name).unwrap()].into());
    let back = read_back(&rule(
        Match::all(vec![subject("a")]),
        vec![keywords("one"), keywords("two")],
    ))
    .unwrap();
    let both: BTreeSet<Keyword> =
        [Keyword::new("one").unwrap(), Keyword::new("two").unwrap()].into();
    assert_eq!(actions(&back), [Action::AddKeywords(both)]);
}

#[test]
fn one_condition_any_reads_back_as_all_and_the_inbox_by_path_as_its_role() {
    let inbox = Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new("INBOX").unwrap()),
        copy: false,
    };
    let back = read_back(&rule(Match::any(vec![subject("a")]), vec![inbox])).unwrap();
    let RuleBody::Typed(typed) = &back.body else {
        panic!("typed");
    };
    assert_eq!(typed.when, Match::all(vec![subject("a")]));
    assert_eq!(
        typed.actions,
        [Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Inbox),
            copy: false
        }]
    );
}

#[test]
fn a_rule_carried_whole_is_not_read_back() {
    let raw = RawRule::new(RuleSyntax::Sieve, "keep;");
    assert_eq!(read_back(&MailRule::opaque(raw)), None);
}

#[test]
fn a_rule_carried_whole_takes_its_facts_from_its_text() {
    // Built by a host that said nothing of what the text does.
    let built = |text: &str| {
        MailRule::opaque(
            RawRule::new(RuleSyntax::Sieve, text)
                .requiring(Vec::new())
                .sending_no_mail(),
        )
    };
    let raw = |rule: MailRule| match facts_from_text(rule).body {
        RuleBody::Opaque(raw) => raw,
        _ => panic!("still carried whole"),
    };
    let by_id = raw(built("fileinto :mailboxid \"a\" \"Other\";"));
    assert!(by_id.names_folders_by_id());
    assert!(
        by_id
            .requires()
            .is_some_and(|needs| needs.contains("mailboxid"))
    );
    let include = raw(built("include :personal \"more\";"));
    assert!(include.includes_scripts());
    let forward = raw(built("redirect \"away@example.com\";"));
    assert!(
        forward.sends_mail(),
        "what the text does, not what the host said"
    );
    // Not one command: nothing is known, so no target stores it.
    let two = raw(built("keep;\r\nstop;"));
    assert_eq!(two.requires(), None);
    // A typed rule is left as it is.
    let typed = rule(Match::all(vec![subject("a")]), vec![Action::Discard]);
    assert_eq!(facts_from_text(typed.clone()), typed);
}
