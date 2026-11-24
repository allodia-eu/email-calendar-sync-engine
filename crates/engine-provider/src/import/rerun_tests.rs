//! Running an import again: a rule the target already holds is not written again, compared as
//! the target reads it back, and a rule carried whole is planned with the facts its text gives.

use engine_core::rules::{
    Action, ForwardingRules, Kept, MailRule, RuleAddress, RuleBody, RuleFeature,
};

use super::{
    ImportDialect, RulesStored,
    tests::{Target, account, folders, into, redirect, rule, vocabulary},
};
use crate::import::RulesImporter;

#[tokio::test]
async fn a_rule_the_target_already_holds_is_not_written_again() {
    let disabled_forward = rule("forward", vec![redirect()]).disabled();
    let own = vec![
        rule("same", vec![into("x"), into("z")]),
        // As a first import left it: disabled for review.
        disabled_forward,
    ];
    let target = Target::with_own(own.clone());
    let imported = vec![
        rule("same", vec![into("x"), into("z")]),
        rule("same", vec![into("x"), into("z")]),
        rule("forward", vec![redirect()]),
        rule("new", vec![into("y")]),
        // Not the same: one action twice is not two different ones.
        rule("same", vec![into("x"), into("x")]),
    ];
    let receipt = target
        .import_rules(&account(), &imported, ForwardingRules::Disabled, &folders())
        .await
        .unwrap();
    // One held `same` answers for one of the two handed in; the forward is held as planned.
    assert_eq!(receipt.already_held, [0, 2]);
    let written = &target.replaced()[0].1;
    assert_eq!(&written[..2], &own[..]);
    assert_eq!(written[2..].len(), 3, "{written:?}");
    assert_eq!(written[2].name, imported[1].name);
    assert_eq!(written[3].name, imported[3].name);
    assert_eq!(
        written[4], imported[4],
        "one action twice is a rule of its own"
    );
    assert_eq!(
        receipt.outcomes.len(),
        5,
        "every rule handed in has its outcome"
    );

    // Run again over what that wrote: nothing is written.
    let again = Target::with_own(written.clone());
    let receipt = again
        .import_rules(&account(), &imported, ForwardingRules::Disabled, &folders())
        .await
        .unwrap();
    assert_eq!(receipt.already_held, [0, 1, 2, 3, 4]);
    assert_eq!(receipt.stored, RulesStored::Unwritten);
    assert!(again.replaced().is_empty(), "nothing written");
}

#[tokio::test]
async fn a_rule_carried_whole_is_never_taken_as_already_held() {
    let raw = engine_core::rules::RawRule::new(engine_core::rules::RuleSyntax::Sieve, "keep;")
        .requiring(Vec::new())
        .sending_no_mail()
        .naming_no_folder_by_id()
        .including_no_script();
    let mut target = Target::with_own(vec![MailRule::opaque(raw.clone())]);
    target.vocabulary = vocabulary().with_opaque(engine_core::rules::RuleSyntax::Sieve, Vec::new());
    let receipt = target
        .import_rules(
            &account(),
            &[MailRule::opaque(raw)],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert!(receipt.already_held.is_empty(), "text is never compared");
    assert_eq!(target.replaced()[0].1.len(), 2);
}

#[tokio::test]
async fn a_rule_is_known_as_held_in_the_form_the_target_reads_it_back() {
    // A language that reads a forward back as a redirect keeping a copy, as Sieve does.
    fn read_back(rule: &MailRule) -> MailRule {
        let mut back = rule.clone();
        back.origin = None;
        if let RuleBody::Typed(typed) = &mut back.body {
            for action in &mut typed.actions {
                if let Action::Forward { to } = action {
                    *action = Action::Redirect {
                        to: to.clone(),
                        copy: true,
                    };
                }
            }
        }
        back
    }
    let forward = rule(
        "forward",
        vec![Action::Forward {
            to: vec![RuleAddress::new("away@example.com").unwrap()],
        }],
    );
    let mut target = Target::with_own(vec![read_back(&forward).disabled()]);
    target.vocabulary = vocabulary().with_exact([RuleFeature::RedirectCopy]).with(
        RuleFeature::Forward,
        engine_core::rules::Support::Approximated,
    );
    target.dialect = ImportDialect::new(|rule| rule, |rule| Some(read_back(rule)));
    let receipt = target
        .import_rules(
            &account(),
            std::slice::from_ref(&forward),
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.already_held, [0]);
    assert!(target.replaced().is_empty(), "nothing written");

    // Compared as handed in, it would be written again.
    target.dialect = ImportDialect::as_given();
    let receipt = target
        .import_rules(
            &account(),
            &[forward],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert!(receipt.already_held.is_empty(), "compared as given");
}

#[tokio::test]
async fn a_rule_carried_whole_is_planned_with_the_facts_its_text_gives() {
    // The language reads that the text runs a script, whatever the host said.
    // The host vouches that it runs no script.
    let raw = engine_core::rules::RawRule::new(engine_core::rules::RuleSyntax::Sieve, "include x;")
        .requiring(Vec::new())
        .sending_no_mail()
        .naming_no_folder_by_id()
        .including_no_script();
    let mut target = Target::with_own(Vec::new());
    target.vocabulary = vocabulary().with_opaque(engine_core::rules::RuleSyntax::Sieve, Vec::new());
    target.dialect = ImportDialect::new(
        |mut rule| {
            if let RuleBody::Opaque(raw) = &rule.body {
                // Rebuilt from its text alone: what it does is not vouched for.
                rule.body = RuleBody::Opaque(
                    engine_core::rules::RawRule::new(raw.syntax(), raw.text())
                        .requiring(Vec::new())
                        .sending_no_mail()
                        .naming_no_folder_by_id(),
                );
            }
            rule
        },
        |rule| Some(rule.clone()),
    );
    let receipt = target
        .import_rules(
            &account(),
            &[MailRule::opaque(raw)],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert!(target.replaced().is_empty(), "nothing written");
}
