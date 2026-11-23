//! The vocabulary's promise, over many generated rule sets: every rule `plan_rules` keeps under
//! an account's vocabulary, the writer writes, and what it writes reads back as typed rules.
//! Seeded, so a failure reproduces.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, ForwardingRules, HeaderField, Kept, MailRule,
        MailboxPath, Match, MeField, NativeAction, NativeCondition, NativeKind, PlanMode,
        RuleAddress, RuleBody, RuleMailbox, RuleName, RuleVocabulary, SizeBound, TextOp, TextTest,
        plan_rules,
    },
};
use engine_sieve::dialect::{read_rules, vocabulary, write_rules};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn chance(&mut self) -> bool {
        self.below(2) == 0
    }
}

const VALUES: [&str; 7] = [
    "plain",
    "q\"uo\\te",
    "two\r\nlines",
    "Größe",
    "${x}",
    "*glob?",
    ".dot",
];

fn text(rng: &mut Rng) -> TextTest {
    let op = [TextOp::Contains, TextOp::Is, TextOp::Matches, TextOp::Regex][rng.below(4)];
    let values = (0..=rng.below(2))
        .map(|_| VALUES[rng.below(VALUES.len())].to_owned())
        .collect();
    TextTest::new(op, values).unwrap()
}

fn condition(rng: &mut Rng) -> Condition {
    match rng.below(10) {
        0 => Condition::Header {
            field: HeaderField::Subject,
            test: text(rng),
        },
        1 => Condition::Header {
            field: HeaderField::named("X-Mailer"),
            test: text(rng),
        },
        2 => Condition::Address {
            field: [
                AddressField::From,
                AddressField::To,
                AddressField::Cc,
                AddressField::ToOrCc,
            ][rng.below(4)],
            part: [
                AddressPart::All,
                AddressPart::LocalPart,
                AddressPart::Domain,
            ][rng.below(3)],
            test: text(rng),
        },
        3 => Condition::Body(text(rng)),
        4 => Condition::Size(SizeBound::Over(1 + rng.next() % (1 << 31))),
        5 => Condition::Size(SizeBound::Under(1 << (rng.below(4) * 10))),
        6 => Condition::HasAttachment,
        7 => Condition::Importance(Importance::High),
        8 => Condition::SentToMe(MeField::To),
        _ => Condition::Native(NativeCondition::new(NativeKind::Graph, "{}")),
    }
}

fn address(rng: &mut Rng) -> RuleAddress {
    RuleAddress::new(format!("a{}@example.com", rng.below(9))).unwrap()
}

fn action(rng: &mut Rng) -> Action {
    match rng.below(11) {
        0 | 1 => Action::FileInto {
            mailbox: match rng.below(4) {
                0 => RuleMailbox::Path(MailboxPath::new("Board").unwrap()),
                1 => RuleMailbox::Role(MailboxRole::Inbox),
                2 => RuleMailbox::Role(MailboxRole::Junk),
                _ => RuleMailbox::Id(MailboxId::try_from("m1").unwrap()),
            },
            copy: rng.chance(),
        },
        2 => Action::Discard,
        3 => Action::Redirect {
            to: (0..=rng.below(2)).map(|_| address(rng)).collect(),
            copy: rng.chance(),
        },
        4 => Action::Forward {
            to: vec![address(rng)],
        },
        5 => Action::AddKeywords(
            [
                Keyword::system(SystemKeyword::Seen),
                Keyword::new("$label1").unwrap(),
            ][..=rng.below(2)]
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
        ),
        6 => Action::AddKeywords([Keyword::system(SystemKeyword::Flagged)].into()),
        7 => Action::MarkImportance(Importance::Low),
        8 => Action::Native(NativeAction::new(NativeKind::Graph, "{}")),
        9 => Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Other("x".into())),
            copy: false,
        },
        _ => Action::Redirect {
            to: Vec::new(),
            copy: false,
        },
    }
}

fn rule(rng: &mut Rng) -> MailRule {
    let conditions = (0..rng.below(3)).map(|_| condition(rng)).collect();
    let mut when = if rng.chance() {
        Match::all(conditions)
    } else {
        Match::any(conditions)
    };
    if rng.below(3) == 0 {
        when = when.except((0..=rng.below(1)).map(|_| condition(rng)).collect());
    }
    let mut rule = MailRule::typed(when, (0..rng.below(4)).map(|_| action(rng)).collect());
    if rng.chance() {
        rule = rule.named(RuleName::new(format!("Rule {}", rng.below(99))).unwrap());
    }
    if rng.below(4) == 0 {
        rule = rule.disabled();
    }
    if rng.chance() {
        rule = rule.stopping();
    }
    rule
}

#[test]
fn every_rule_the_plan_keeps_is_written_and_reads_back_typed() {
    let targets: [(&str, RuleVocabulary); 3] = [
        (
            "Stalwart-like",
            vocabulary(
                [
                    "fileinto",
                    "copy",
                    "regex",
                    "body",
                    "imap4flags",
                    "editheader",
                    "special-use",
                    "mailboxid",
                    "mime",
                ],
                Some(1),
            ),
        ),
        (
            "Dovecot-like",
            vocabulary(
                ["fileinto", "copy", "regex", "body", "imap4flags", "mime"],
                Some(4),
            ),
        ),
        ("base Sieve", vocabulary([], None)),
    ];
    let mut rng = Rng(0x0dd_ba11_cafe_f00d);
    let mut written_rules = 0;
    for round in 0..3_000 {
        let rules: Vec<MailRule> = (0..=rng.below(4)).map(|_| rule(&mut rng)).collect();
        let mode = if rng.chance() {
            PlanMode::Edit
        } else {
            PlanMode::Import {
                forwarding: ForwardingRules::Disabled,
            }
        };
        for (name, target) in &targets {
            let plan = plan_rules(&rules, target, mode);
            let kept = plan
                .outcomes
                .iter()
                .filter(|outcome| outcome.kept != Kept::NotKept)
                .count();
            assert_eq!(kept, plan.rules.len(), "round {round}: {name}");
            let script = write_rules("", &plan.written())
                .unwrap_or_else(|err| panic!("round {round}: {name}: {err}: {:?}", plan.rules));
            let read = read_rules(&script);
            assert_eq!(
                read.len(),
                plan.rules.len(),
                "round {round}: {name}: {script:?}"
            );
            for back in &read {
                assert!(
                    matches!(back.body, RuleBody::Typed(_)),
                    "round {round}: {name}: read back whole: {script:?}"
                );
            }
            written_rules += read.len();
        }
    }
    assert!(
        written_rules > 1_000,
        "the generator keeps rules: {written_rules}"
    );
}
