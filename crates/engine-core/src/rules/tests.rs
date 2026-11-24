use std::collections::BTreeSet;

use super::*;
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
    raw::RawRules,
    version::RuleSetVersion,
};

fn text(op: TextOp, value: &str) -> TextTest {
    TextTest::new(op, vec![value.to_owned()]).unwrap()
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
                mailbox: RuleMailbox::Path(MailboxPath::new("secret-folder").unwrap()),
                copy: true,
            },
            Action::Redirect {
                to: vec![RuleAddress::new("secret@example.com").unwrap()],
                copy: false,
            },
            Action::AddKeywords(keywords),
            Action::Native(NativeAction::new(NativeKind::Gmail, "{\"secret\":2}")),
        ],
    )
    .named(RuleName::new("secret-name").unwrap())
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
        .named(RuleName::new("Spam").unwrap())
        .disabled()
        .stopping()
        .with_origin(RuleOrigin::ScriptSegment {
            script: 7,
            index: 3,
        });
    assert_eq!(rule.name.as_ref().map(RuleName::as_str), Some("Spam"));
    assert!(!rule.enabled);
    assert_eq!(
        rule.origin,
        Some(RuleOrigin::ScriptSegment {
            script: 7,
            index: 3
        })
    );
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
    assert_eq!(MailboxPath::new("a").unwrap().as_str(), "a");
    assert_eq!(
        RuleAddress::new("b@example.com").unwrap().as_str(),
        "b@example.com"
    );
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
        .with_opaque(RuleSyntax::Sieve, ["fileinto".to_owned()])
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
    let raw = RawRule::new(RuleSyntax::Sieve, "keep;");
    assert!(
        !vocabulary.stores_opaque(&raw),
        "what it requires is not known"
    );
    assert!(vocabulary.stores_opaque(&raw.clone().requiring(["fileinto".to_owned()])));
    assert!(!vocabulary.stores_opaque(&raw.requiring(["regex".to_owned()])));
    assert!(vocabulary.stores_native(NativeKind::Graph));
    assert!(!vocabulary.stores_native(NativeKind::Gmail));
    assert_eq!(RuleVocabulary::new().max_redirects(), None);
}

#[test]
fn a_receipt_carries_its_version_and_outcomes() {
    let receipt = RulesReceipt::new(Some(RuleSetVersion::new("s2")), Vec::new());
    assert_eq!(
        (receipt.stored, receipt.reply),
        (RulesStored::InPlace, ReplyEffect::Untouched)
    );
    let created = receipt
        .clone()
        .with_stored(RulesStored::CreatedRunning)
        .with_reply(ReplyEffect::Stopped);
    assert_eq!(
        (created.stored, created.reply),
        (RulesStored::CreatedRunning, ReplyEffect::Stopped)
    );
    assert_eq!(RuleSet::default().activation, RulesActivation::Running);
    assert_eq!(
        RuleSet::new(Vec::new())
            .with_activation(RulesActivation::Inactive)
            .activation,
        RulesActivation::Inactive
    );
    assert_eq!(
        receipt.version.as_ref().map(RuleSetVersion::as_str),
        Some("s2")
    );
    assert_eq!(receipt.outcomes, Vec::new());
}

#[test]
fn a_receipt_lists_each_rule_a_rule_by_rule_write_did_not_store() {
    use crate::error::FailureClass;

    let receipt = RulesReceipt::new(None, Vec::new());
    assert_eq!(receipt.failures.len(), 0);
    let failed = receipt.with_failures(vec![
        RuleWriteFailure::new(FailedRule::Written(2), FailureClass::Permanent),
        RuleWriteFailure::new(FailedRule::Removed(0), FailureClass::Retryable).outcome_unknown(),
    ]);
    assert_eq!(failed.failures[0].rule, FailedRule::Written(2));
    assert!(!failed.failures[0].outcome_unknown);
    assert_eq!(failed.failures[1].class, FailureClass::Retryable);
    assert!(failed.failures[1].outcome_unknown);
}

#[test]
fn debug_of_each_action_names_it_and_counts_its_values() {
    let shown = format!(
        "{:?}",
        [
            Action::Discard,
            Action::Forward {
                to: vec![RuleAddress::new("secret@example.com").unwrap()],
            },
            Action::MarkImportance(Importance::High),
        ]
    );
    assert_eq!(shown, "[Discard, Forward { to: 1 }, MarkImportance(High)]");
}

#[test]
fn a_rule_value_is_never_empty_and_never_spans_lines() {
    assert_eq!(RuleName::new(""), Err(RuleValueError::Empty));
    for bad in ["a\nb", "a\rb", "a\0b"] {
        assert_eq!(RuleName::new(bad), Err(RuleValueError::LineBreak));
        assert_eq!(MailboxPath::new(bad), Err(RuleValueError::LineBreak));
        assert_eq!(RuleAddress::new(bad), Err(RuleValueError::LineBreak));
    }
    assert_eq!(
        TextTest::new(TextOp::Is, Vec::new()).unwrap_err(),
        RuleValueError::Empty
    );
    let test = TextTest::new(TextOp::Regex, vec!["a".to_owned()]).unwrap();
    assert_eq!(
        (test.op(), test.values()),
        (TextOp::Regex, &["a".to_owned()][..])
    );
    assert!(RuleValueError::Empty.to_string().contains("empty"));
    assert!(RuleValueError::LineBreak.to_string().contains("line break"));
}

#[test]
fn a_named_header_shows_its_length_not_its_name() {
    let debug = format!("{:?}", HeaderField::Named("X-Secret".to_owned()));
    assert_eq!(debug, "Named { len: 8 }");
    assert_eq!(format!("{:?}", HeaderField::Subject), "Subject");
    assert!(HeaderField::Named("sUbJeCt".to_owned()).is_subject());
    let raw = format!(
        "{:?}",
        RawRule::new(RuleSyntax::Sieve, "secret").named(RuleName::new("secret").unwrap())
    );
    assert!(!raw.contains("secret"), "{raw}");
}

#[test]
fn a_receipt_says_whether_the_rules_it_wrote_run() {
    let receipt = RulesReceipt::new(None, Vec::new());
    assert_eq!(receipt.activation, RulesActivation::Running);
    let receipt = receipt.with_activation(RulesActivation::Inactive);
    assert_eq!(receipt.activation, RulesActivation::Inactive);
    assert!(receipt.already_held.is_empty(), "nothing for an edit");
    let receipt = receipt.with_already_held(vec![2]);
    assert_eq!(receipt.already_held, [2]);
}

#[test]
fn a_feature_taken_out_is_held_no_longer() {
    let vocabulary = RuleVocabulary::new()
        .with_exact([RuleFeature::FileIntoId, RuleFeature::Stop])
        .without(RuleFeature::FileIntoId);
    assert_eq!(vocabulary.support(RuleFeature::FileIntoId), None);
    assert_eq!(vocabulary.support(RuleFeature::Stop), Some(Support::Exact));
}
