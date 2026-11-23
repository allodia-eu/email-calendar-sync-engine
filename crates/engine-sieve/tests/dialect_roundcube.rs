//! The dialect against scripts Roundcube's managesieve plugin wrote
//! (`fixtures/roundcube/README.md`): what reads typed, what is carried whole, that the writer
//! writes the plugin's bytes, and that an edit to one rule leaves every other segment as it was.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, MailboxRole},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, MailboxPath, Match,
        MatchMode, RuleAddress, RuleBody, RuleMailbox, RuleName, RuleOrigin, SizeBound, TextOp,
        TextTest,
    },
};
use engine_sieve::{
    dialect::{DialectErrorKind, read_rules, write_rules},
    segment,
};

const ONE_RULE: &str = include_str!("fixtures/roundcube/one_rule.sieve");
const DISABLED: &str = include_str!("fixtures/roundcube/disabled_rule.sieve");
const MANY: &str = include_str!("fixtures/roundcube/many_rules.sieve");
const QUOTING: &str = include_str!("fixtures/roundcube/quoting.sieve");
const VACATION: &str = include_str!("fixtures/roundcube/vacation_rule.sieve");

fn test(op: TextOp, values: &[&str]) -> TextTest {
    TextTest::new(op, values.iter().map(|value| (*value).to_owned()).collect()).unwrap()
}

fn subject(op: TextOp, value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: test(op, &[value]),
    }
}

fn into(path: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new(path).unwrap()),
        copy: false,
    }
}

fn unplaced(mut rule: MailRule) -> MailRule {
    rule.origin = None;
    rule
}

fn names(rules: &[MailRule]) -> Vec<Option<&str>> {
    rules
        .iter()
        .map(|rule| rule.name.as_ref().map(RuleName::as_str))
        .collect()
}

/// The fingerprint the reader gives `script`, as a rule read from it carries.
fn fingerprint_of(script: &str) -> u64 {
    match read_rules(script)[0].origin {
        Some(RuleOrigin::ScriptSegment { script, .. }) => script,
        _ => unreachable!(),
    }
}

fn is_opaque(rule: &MailRule) -> bool {
    matches!(rule.body, RuleBody::Opaque(_))
}

#[test]
fn the_plugin_s_rules_read_typed_and_what_the_dialect_does_not_write_is_carried_whole() {
    let rules = read_rules(MANY);
    assert_eq!(
        names(&rules),
        [
            Some("Any of two"),
            Some("All of two"),
            Some("Big"),
            None,
            Some("Regex"),
            Some("Everything")
        ]
    );
    let flags: BTreeSet<Keyword> = ["\\Flagged", "\\Seen", "$label1"]
        .into_iter()
        .map(|flag| Keyword::from_flag(flag).unwrap())
        .collect();
    let expected = [
        MailRule::typed(
            Match::any(vec![
                Condition::Header {
                    field: HeaderField::Subject,
                    test: test(TextOp::Contains, &["urgent", "asap"]),
                },
                Condition::Address {
                    field: AddressField::ToOrCc,
                    part: AddressPart::Domain,
                    test: test(TextOp::Is, &["example.org"]),
                },
            ]),
            vec![Action::AddKeywords(flags)],
        )
        .named(RuleName::new("Any of two").unwrap()),
        MailRule::typed(
            Match::all(vec![subject(TextOp::Matches, "report*")]).except(vec![Condition::Header {
                field: HeaderField::Named("x-spam-flag".to_owned()),
                test: test(TextOp::Is, &["YES"]),
            }]),
            vec![Action::Redirect {
                to: vec![RuleAddress::new("archive@example.com").unwrap()],
                copy: true,
            }],
        )
        .named(RuleName::new("All of two").unwrap()),
        MailRule::typed(
            Match::all(vec![Condition::Size(SizeBound::Over(5 << 20))]),
            vec![Action::Discard],
        )
        .named(RuleName::new("Big").unwrap()),
        MailRule::typed(
            Match::all(vec![Condition::Body(test(
                TextOp::Contains,
                &["unsubscribe"],
            ))]),
            vec![into("Lists")],
        )
        .stopping(),
    ];
    for (i, want) in expected.into_iter().enumerate() {
        assert_eq!(
            rules[i].origin,
            Some(RuleOrigin::ScriptSegment {
                script: fingerprint_of(MANY),
                index: i + 1
            }),
            "rule {i}"
        );
        assert_eq!(unplaced(rules[i].clone()), want, "rule {i}");
    }
    // `setflag` replaces flags and `keep` is no neutral action: both carried whole, named.
    assert!(is_opaque(&rules[4]) && is_opaque(&rules[5]));
}

#[test]
fn a_disabled_rule_reads_its_test_from_the_comment() {
    let rules = read_rules(DISABLED);
    assert!(rules.iter().all(|rule| !rule.enabled && !is_opaque(rule)));
    let RuleBody::Typed(newsletters) = &rules[1].body else {
        unreachable!()
    };
    assert_eq!(newsletters.when.mode, MatchMode::All);
    assert_eq!(newsletters.when.conditions.len(), 1);
    assert_eq!(newsletters.when.except.len(), 1);
    assert_eq!(
        newsletters.actions,
        [into("Trash")],
        "a folder named Trash is a path, as the plugin wrote it"
    );
}

#[test]
fn quoting_and_text_blocks_read_as_the_values_the_plugin_was_given() {
    let rules = read_rules(QUOTING);
    assert_eq!(
        rules[0].name.as_ref().unwrap().as_str(),
        "Quote \"and\" back\\slash"
    );
    assert_eq!(
        unplaced(rules[0].clone()),
        MailRule::typed(
            Match::all(vec![subject(TextOp::Contains, "say \"hi\" \\ there")]),
            vec![into("Odd \"folder\"")],
        )
        .named(RuleName::new("Quote \"and\" back\\slash").unwrap())
    );
    assert!(is_opaque(&rules[1]), "reject is no neutral action");
    assert_eq!(
        unplaced(rules[2].clone()),
        MailRule::typed(
            Match::all(vec![subject(TextOp::Contains, "Größe")]),
            vec![into("Ärger")]
        )
        .named(RuleName::new("Unicode é").unwrap())
    );
    let rules = read_rules(VACATION);
    assert!(is_opaque(&rules[0]) && !is_opaque(&rules[1]));
}

#[test]
fn the_writer_writes_the_plugin_s_bytes() {
    let ours = |script: &str| {
        let rules: Vec<MailRule> = read_rules(script).into_iter().map(unplaced).collect();
        write_rules("", &rules).unwrap()
    };
    assert_eq!(ours(ONE_RULE), ONE_RULE);
    assert_eq!(ours(DISABLED), DISABLED);
    // A text block in a test, and a quoted value with escapes, as the plugin wrote them.
    let quoting = segment(QUOTING).unwrap();
    let typed: Vec<MailRule> = read_rules(QUOTING)
        .into_iter()
        .filter(|rule| !is_opaque(rule))
        .map(unplaced)
        .collect();
    let expected = format!(
        "require [\"fileinto\"];\r\n{}{}",
        quoting[1].text(),
        quoting[3].text()
    );
    assert_eq!(write_rules("", &typed).unwrap(), expected);
}

#[test]
fn a_script_written_over_with_its_own_rules_is_unchanged() {
    for script in [ONE_RULE, DISABLED, MANY, QUOTING, VACATION] {
        assert_eq!(write_rules(script, &read_rules(script)).unwrap(), script);
    }
}

#[test]
fn an_edit_to_one_rule_leaves_every_other_segment_byte_identical() {
    let mut rules = read_rules(MANY);
    let RuleBody::Typed(big) = &mut rules[2].body else {
        unreachable!()
    };
    big.when.conditions = vec![Condition::Size(SizeBound::Over(10 << 20))];
    let written = write_rules(MANY, &rules).unwrap();
    let before = segment(MANY).unwrap();
    let after = segment(&written).unwrap();
    assert_eq!(before.len(), after.len());
    for (i, (was, now)) in before.iter().zip(&after).enumerate() {
        if i == 3 {
            assert_eq!(
                now.text(),
                "# rule:[Big]\r\nif size :over 10M\r\n{\r\n\tdiscard;\r\n}\r\n"
            );
        } else {
            assert_eq!(was.text(), now.text(), "segment {i}");
        }
    }
    // Rules removed and reordered: the others are copied as they were.
    let rules = read_rules(MANY);
    let written = write_rules(MANY, &[rules[5].clone(), rules[0].clone()]).unwrap();
    let after = segment(&written).unwrap();
    assert_eq!(after[0].text(), before[0].text());
    assert_eq!(after[1].text(), before[6].text());
    assert_eq!(after[2].text(), before[1].text());
}

#[test]
fn a_rule_that_needs_an_extension_rewrites_only_the_require() {
    let mut rules = read_rules(ONE_RULE);
    rules.push(MailRule::typed(
        Match::all(vec![subject(TextOp::Regex, "^x")]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::Junk),
            copy: true,
        }],
    ));
    let written = write_rules(ONE_RULE, &rules).unwrap();
    let before = segment(ONE_RULE).unwrap();
    let after = segment(&written).unwrap();
    assert_eq!(
        after[0].text(),
        "require [\"copy\",\"fileinto\",\"regex\",\"special-use\"];\r\n"
    );
    assert_eq!(after[1].text(), before[1].text());
    assert_eq!(
        after[2].text(),
        "if header :regex \"subject\" \"^x\"\r\n{\r\n\tfileinto :copy :specialuse \"\\\\Junk\" \"Junk\";\r\n}\r\n"
    );
    assert_eq!(
        unplaced(read_rules(&written)[1].clone()),
        unplaced(rules[1].clone())
    );
}

#[test]
fn a_rule_s_name_is_its_last_name_line_and_a_rewrite_keeps_its_other_comments() {
    let script = "# rule:[Old]\r\n# kept: why this rule exists\r\n# rule:[Current]\r\nif true\r\n{\r\n\tdiscard;\r\n}\r\n";
    let mut rules = read_rules(script);
    assert_eq!(rules[0].name.as_ref().unwrap().as_str(), "Current");
    rules[0].name = Some(RuleName::new("Renamed").unwrap());
    assert_eq!(
        write_rules(script, &rules).unwrap(),
        "# kept: why this rule exists\r\n# rule:[Renamed]\r\nif true\r\n{\r\n\tdiscard;\r\n}\r\n"
    );
}

#[test]
fn a_name_line_after_the_last_rule_names_no_rule_written_after_it() {
    let script = "keep;\r\n# rule:[Orphan]\r\n";
    let rules = read_rules(script);
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, None);
    let mut more = rules.clone();
    more.push(
        MailRule::typed(Match::default(), vec![Action::Discard])
            .named(RuleName::new("New").unwrap()),
    );
    let written = write_rules(script, &more).unwrap();
    assert_eq!(
        written,
        "keep;\r\n# rule:[New]\r\nif true\r\n{\r\n\tdiscard;\r\n}\r\n# rule:[Orphan]\r\n"
    );
    assert_eq!(names(&read_rules(&written)), [None, Some("New")]);
    // A rule written after one with no line end starts on a line of its own.
    let written = write_rules("keep;", &more).unwrap();
    assert!(written.starts_with("keep;\r\n# rule:[New]"), "{written:?}");
}

#[test]
fn a_script_that_cannot_be_split_is_read_whole_and_not_written_over() {
    let rules = read_rules("if true { keep;");
    assert_eq!(rules.len(), 1);
    assert!(is_opaque(&rules[0]) && rules[0].origin.is_none());
    let err = write_rules("if true { keep;", &rules).unwrap_err();
    assert!(matches!(err.kind(), DialectErrorKind::Unsplittable(_)));
    assert!(err.to_string().contains("rule by rule"));
    assert_eq!(read_rules(""), Vec::new());
    assert!(matches!(
        read_rules("require 1;")[0].body,
        RuleBody::Opaque(_)
    ));
}

#[test]
fn a_value_with_a_variable_where_variables_are_required_is_carried_whole() {
    let typed = "if header :contains \"subject\" \"${x}\"\r\n{\r\n\tdiscard;\r\n}\r\n";
    assert!(!is_opaque(&read_rules(typed)[0]));
    assert!(is_opaque(
        &read_rules(&format!("require \"variables\";\r\n{typed}"))[0]
    ));
}

#[test]
fn what_the_writer_cannot_write_is_refused_by_index() {
    let opaque = read_rules(MANY).remove(4);
    let mut renamed = opaque.clone();
    renamed.name = Some(RuleName::new("Renamed").unwrap());
    renamed.origin = None;
    let mut disabled = opaque.clone().disabled();
    disabled.origin = None;
    let unknown = MailRule::opaque(engine_core::rules::RawRule::new(
        engine_core::rules::RuleSyntax::Sieve,
        "keep;\r\n",
    ));
    let unrenderable = [
        renamed,
        disabled,
        unknown,
        MailRule::typed(
            Match::default(),
            vec![Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Other("x".into())),
                copy: false,
            }],
        ),
        MailRule::typed(
            Match::all(vec![subject(TextOp::Is, "bare\nline")]),
            vec![Action::Discard],
        ),
        MailRule::typed(Match::default(), vec![Action::AddKeywords(BTreeSet::new())]),
        MailRule::typed(
            Match::default(),
            vec![Action::Redirect {
                to: Vec::new(),
                copy: false,
            }],
        ),
    ];
    for (i, rule) in unrenderable.into_iter().enumerate() {
        let rules = [
            MailRule::typed(Match::default(), vec![Action::Discard]),
            rule,
        ];
        let err = write_rules("", &rules).unwrap_err();
        assert_eq!(
            err.kind(),
            DialectErrorKind::Unrenderable { rule: 1 },
            "case {i}"
        );
        assert!(err.to_string().contains("rule 1"));
    }
}
