//! The vocabulary's promise, over many generated rule sets: every rule `plan_rules` keeps under
//! an account's vocabulary the writer writes, and what it writes reads back meaning what the
//! plan meant, modulo the documented normalisations ([`normal`]). Typed and carried rules, edit
//! and both import modes, over a script that reads `${` literally and one that expands it.
//! Seeded, so a failure reproduces.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, ForwardingRules, HeaderField, Kept, MailRule,
        MailboxPath, Match, MatchMode, MeField, NativeAction, NativeCondition, NativeKind,
        PlanMode, RuleAddress, RuleBody, RuleMailbox, RuleName, RuleVocabulary, SizeBound, TextOp,
        TextTest, plan_rules,
    },
};
use engine_sieve::dialect::{DialectErrorKind, read_rules, vocabulary, write_rules};

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

/// Rules carried whole, read from scripts of their own: what they need and do is the reader's.
fn carried_rules() -> Vec<MailRule> {
    [
        "keep;\r\n",
        "require \"envelope\";\r\nif envelope :is \"from\" \"a@example.com\" { redirect \"b@example.net\"; }\r\n",
        "require \"vacation\";\r\n# rule:[Away]\r\nvacation \"away\";\r\n",
        "if header :contains \"subject\" \"${x}\" { keep; }\r\n",
    ]
    .iter()
    .map(|script| read_rules(script).pop().unwrap())
    .collect()
}

/// A rule as the reader gives it back, for comparing: the documented normalisations applied.
/// Any of at most one condition is all of it; Exchange's forward is a redirect that keeps a
/// copy; redirects are one action per run of the same `:copy`; keywords are one set; marks come
/// before deliveries; an origin is where it was read, not what it is.
fn normal(rule: &MailRule) -> MailRule {
    let mut rule = rule.clone();
    rule.origin = None;
    if let RuleBody::Typed(typed) = &mut rule.body {
        if typed.when.conditions.len() <= 1 {
            typed.when.mode = MatchMode::All;
        }
        let mut keywords = BTreeSet::new();
        let mut marks = Vec::new();
        let mut deliveries: Vec<Action> = Vec::new();
        let redirect = |deliveries: &mut Vec<Action>, to: &[RuleAddress], copy: bool| {
            for address in to {
                match deliveries.last_mut() {
                    Some(Action::Redirect { to, copy: was }) if *was == copy => {
                        to.push(address.clone());
                    }
                    _ => deliveries.push(Action::Redirect {
                        to: vec![address.clone()],
                        copy,
                    }),
                }
            }
        };
        for action in &typed.actions {
            match action {
                Action::AddKeywords(set) => keywords.extend(set.iter().cloned()),
                Action::MarkImportance(_) => marks.push(action.clone()),
                Action::Forward { to } => redirect(&mut deliveries, to, true),
                Action::Redirect { to, copy } => redirect(&mut deliveries, to, *copy),
                other => deliveries.push(other.clone()),
            }
        }
        let mut actions = Vec::new();
        if !keywords.is_empty() {
            actions.push(Action::AddKeywords(keywords));
        }
        actions.extend(marks);
        actions.extend(deliveries);
        typed.actions = actions;
    }
    rule
}

/// Whether `rule` holds `${` where a script would read it: a typed value, or a carried text.
fn holds_expansion(rule: &MailRule) -> bool {
    match &rule.body {
        RuleBody::Opaque(raw) => raw.expansion().is_some(),
        RuleBody::Typed(typed) => typed.when.conditions.iter().chain(&typed.when.except).any(
            |condition| match condition {
                Condition::Header { test, .. }
                | Condition::Address { test, .. }
                | Condition::Body(test) => test.values().iter().any(|value| value.contains("${")),
                _ => false,
            },
        ),
        _ => false,
    }
}

#[test]
fn every_rule_the_plan_keeps_is_written_and_reads_back_as_it_meant() {
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
                    "envelope",
                    "vacation",
                    "variables",
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
    let modes = [
        PlanMode::Edit,
        PlanMode::Import {
            forwarding: ForwardingRules::Disabled,
        },
        PlanMode::Import {
            forwarding: ForwardingRules::Enabled,
        },
    ];
    let bases = ["", "require \"variables\";\r\n"];
    let carried = carried_rules();
    let mut rng = Rng(0x0dd_ba11_cafe_f00d);
    let (mut compared, mut refused) = (0, 0);
    for round in 0..2_000 {
        let rules: Vec<MailRule> = (0..=rng.below(4))
            .map(|_| {
                if rng.below(5) == 0 {
                    carried[rng.below(carried.len())].clone()
                } else {
                    rule(&mut rng)
                }
            })
            .collect();
        let mode = modes[rng.below(modes.len())];
        let base = bases[rng.below(bases.len())];
        for (name, target) in &targets {
            let plan = plan_rules(&rules, target, mode);
            let kept = plan
                .outcomes
                .iter()
                .filter(|outcome| outcome.kept != Kept::NotKept)
                .count();
            assert_eq!(kept, plan.rules.len(), "round {round}: {name}");
            let script = match write_rules(base, &plan.written()) {
                Ok(script) => script,
                Err(err) => {
                    // The one refusal the plan cannot foresee: `${` over a script that expands
                    // it differently, which the writer checks against the script it writes over.
                    let DialectErrorKind::Variable { rule } = err.kind() else {
                        panic!("round {round}: {name}: {err}: {:?}", plan.rules);
                    };
                    assert!(
                        holds_expansion(&plan.rules[rule].rule),
                        "round {round}: {name}"
                    );
                    refused += 1;
                    continue;
                }
            };
            let read = read_rules(&script);
            assert_eq!(
                read.len(),
                plan.rules.len(),
                "round {round}: {name}: {script:?}"
            );
            for (planned, back) in plan.rules.iter().zip(&read) {
                match (&planned.rule.body, &back.body) {
                    (RuleBody::Opaque(sent), RuleBody::Opaque(got)) => {
                        assert_eq!(sent.text(), got.text(), "round {round}: {name}");
                        assert_eq!(planned.rule.name, back.name, "round {round}: {name}");
                    }
                    _ => assert_eq!(
                        normal(&planned.rule),
                        normal(back),
                        "round {round}: {name}: {script:?}"
                    ),
                }
                compared += 1;
            }
        }
    }
    assert!(compared > 1_000, "the generator keeps rules: {compared}");
    assert!(refused > 10, "the variables base refuses some: {refused}");
}
