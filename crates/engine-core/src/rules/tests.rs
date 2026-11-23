use std::collections::BTreeSet;

use super::*;
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
    raw::RawRules,
    version::RuleSetVersion,
};

fn text(op: TextOp, value: &str) -> TextTest {
    TextTest::new(op, vec![value.to_owned()])
}

/// One rule of every kind, with a secret in every value a rule can carry.
fn secret_rule() -> MailRule {
    let keywords: BTreeSet<Keyword> = [Keyword::new("secret-label").unwrap()].into();
    MailRule::typed(
        Match::all(vec![
            Condition::Header {
                field: HeaderField::Subject,
                test: text(TextOp::Contains, "secret-subject"),
            },
            Condition::Address {
                field: AddressField::From,
                part: AddressPart::Domain,
                test: text(TextOp::Is, "secret.example"),
            },
            Condition::Body(text(TextOp::Regex, "secret-body")),
            Condition::Native(NativeCondition::new(NativeKind::Graph, "{\"secret\":1}")),
        ])
        .except(vec![Condition::Size(SizeBound::Over(1))]),
        vec![
            Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("secret-folder")),
                copy: true,
            },
            Action::Redirect {
                to: vec![RuleAddress::new("secret@example.com")],
                copy: false,
            },
            Action::AddKeywords(keywords),
            Action::Native(NativeAction::new(NativeKind::Gmail, "{\"secret\":2}")),
        ],
    )
    .named(RuleName::new("secret-name"))
}

#[test]
fn debug_of_every_rule_part_shows_no_value() {
    let set = RuleSet::new(vec![
        secret_rule(),
        MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "keep; # secret")),
    ])
    .with_version(RuleSetVersion::new("v1"))
    .with_raw(RawRules::new("# secret script"));
    let debug = format!("{set:?}");
    assert!(!debug.contains("secret"), "{debug}");
    assert!(
        debug.contains("Contains") && debug.contains("values: 1"),
        "{debug}"
    );
}

#[test]
fn builders_set_what_they_say() {
    let rule = MailRule::typed(Match::default(), vec![Action::Discard])
        .named(RuleName::new("Spam"))
        .disabled()
        .stopping()
        .with_origin(RuleOrigin::ScriptSegment(3));
    assert_eq!(rule.name.as_ref().map(RuleName::as_str), Some("Spam"));
    assert!(!rule.enabled);
    assert_eq!(rule.origin, Some(RuleOrigin::ScriptSegment(3)));
    let RuleBody::Typed(typed) = &rule.body else {
        panic!("typed")
    };
    assert!(typed.stop);
    assert_eq!(typed.when.mode, MatchMode::All);
    assert_eq!(Match::any(Vec::new()).mode, MatchMode::Any);

    let raw = RawRule::new(RuleSyntax::Sieve, "keep;");
    let opaque = MailRule::opaque(raw.clone()).stopping();
    assert_eq!(opaque.body, RuleBody::Opaque(raw.clone()));
    assert_eq!((raw.syntax(), raw.text()), (RuleSyntax::Sieve, "keep;"));
    assert_eq!(MailboxPath::new("a").as_str(), "a");
    assert_eq!(RuleAddress::new("b@example.com").as_str(), "b@example.com");
    let _ = (
        RuleMailbox::Role(MailboxRole::Trash),
        RuleMailbox::Id(MailboxId::try_from("m1").unwrap()),
        Condition::Importance(Importance::High),
        Condition::SentToMe(MeField::OnlyTo),
    );
}

#[test]
fn a_vocabulary_answers_what_it_was_given() {
    let vocabulary = RuleVocabulary::new()
        .with_exact([RuleFeature::FileInto, RuleFeature::Stop])
        .with(RuleFeature::Forward, Support::Approximated)
        .with_max_redirects(1)
        .with_opaque(RuleSyntax::Sieve)
        .with_native(NativeKind::Graph);
    assert_eq!(
        vocabulary.support(RuleFeature::FileInto),
        Some(Support::Exact)
    );
    assert_eq!(
        vocabulary.support(RuleFeature::Forward),
        Some(Support::Approximated)
    );
    assert_eq!(vocabulary.support(RuleFeature::Discard), None);
    assert_eq!(vocabulary.max_redirects(), Some(1));
    assert!(vocabulary.stores_opaque(RuleSyntax::Sieve));
    assert!(vocabulary.stores_native(NativeKind::Graph));
    assert!(!vocabulary.stores_native(NativeKind::Gmail));
    assert_eq!(RuleVocabulary::new().max_redirects(), None);
}

#[test]
fn a_receipt_carries_its_version_and_outcomes() {
    let receipt = RulesReceipt::new(Some(RuleSetVersion::new("s2")), Vec::new());
    assert_eq!(
        receipt.version.as_ref().map(RuleSetVersion::as_str),
        Some("s2")
    );
    assert_eq!(receipt.outcomes, Vec::new());
}

#[test]
fn debug_of_each_action_names_it_and_counts_its_values() {
    let shown = format!(
        "{:?}",
        [
            Action::Discard,
            Action::Forward {
                to: vec![RuleAddress::new("secret@example.com")],
            },
            Action::MarkImportance(Importance::High),
        ]
    );
    assert_eq!(shown, "[Discard, Forward { to: 1 }, MarkImportance(High)]");
}
