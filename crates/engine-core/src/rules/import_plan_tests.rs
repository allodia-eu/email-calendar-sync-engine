//! What an import adds to the plan: a folder named by an id the target does not have, the rules
//! the forwarding switch decides ([`held_forwarding`]), and that a planned import written again as
//! an edit is written as planned.

use super::{
    plan_tests::{IMPORT, into, loss, one, redirect, rich, subject, without},
    *,
};
use crate::{
    ids::MailboxId,
    mail::{Importance, Keyword, SystemKeyword},
};

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

fn into_id(value: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(id(value)),
        copy: false,
    }
}

fn mark_read() -> Action {
    Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into())
}

fn by_id() -> RuleVocabulary {
    rich().with_exact([RuleFeature::FileIntoId])
}

#[test]
fn a_folder_id_the_target_does_not_have_is_a_missing_mailbox() {
    let target = by_id().with_mailboxes([id("finance")]);
    let rule = MailRule::typed(
        Match::all(vec![subject("a")]),
        vec![into_id("unmapped"), mark_read()],
    );
    let (written, outcome) = one(&rule, &target, IMPORT);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Action(0), LossReason::MissingMailbox)]
    );
    let Some(RuleBody::Typed(written)) = written.map(|rule| rule.body) else {
        panic!("the rule is written typed");
    };
    assert_eq!(written.actions, vec![mark_read()]);

    // Filing into it was all it did: nothing is left to do.
    let only = MailRule::typed(Match::all(vec![subject("a")]), vec![into_id("unmapped")]);
    assert_eq!(one(&only, &target, IMPORT).1.kept, Kept::NotKept);
    // A folder the target has is kept, and so is any id where the target's folders are not known.
    let mapped = MailRule::typed(Match::all(vec![subject("a")]), vec![into_id("finance")]);
    assert_eq!(one(&mapped, &target, IMPORT).1.kept, Kept::Whole);
    assert_eq!(one(&only, &by_id(), IMPORT).1.kept, Kept::Whole);
    assert!(target.has_mailbox(&id("finance")));
    assert!(!target.has_mailbox(&id("unmapped")));
    assert!(by_id().has_mailbox(&id("unmapped")));
}

#[test]
fn a_missing_mailbox_is_reported_where_the_target_cannot_file_by_id_at_all_too() {
    // The folder has no counterpart: that is the reason, whatever else the target lacks.
    let target = rich().with_mailboxes([id("finance")]);
    let rule = MailRule::typed(Match::default(), vec![into_id("unmapped"), mark_read()]);
    assert_eq!(
        one(&rule, &target, IMPORT).1.losses,
        vec![loss(RulePart::Action(0), LossReason::MissingMailbox)]
    );
}

/// One rule of each kind an import changes, for a target that takes them in some form.
fn imported() -> Vec<MailRule> {
    let native = NativeAction::new(NativeKind::Graph, "{}");
    vec![
        MailRule::typed(Match::all(vec![subject("a")]), vec![redirect(1)])
            .named(RuleName::new("Forward").unwrap()),
        MailRule::typed(
            Match::all(vec![subject("b")]),
            vec![into("x"), Action::Native(native), into_id("unmapped")],
        )
        .stopping(),
        MailRule::typed(
            Match::all(vec![subject("c")]),
            vec![Action::MarkImportance(Importance::High), into_id("finance")],
        ),
        MailRule::typed(
            Match::all(vec![subject("d")]),
            vec![Action::Forward {
                to: vec![RuleAddress::new("f@example.com").unwrap()],
            }],
        ),
        MailRule::typed(Match::all(vec![subject("e")]), vec![into("y")]).disabled(),
    ]
}

fn approximating() -> RuleVocabulary {
    by_id()
        .with(RuleFeature::MarkImportance, Support::Approximated)
        .with(RuleFeature::Forward, Support::Approximated)
}

#[test]
fn a_planned_import_written_again_as_an_edit_is_written_as_planned() {
    // An importer writes the planned rules through the target's own edit, which plans them
    // again: that second plan must keep every one exactly as the first wrote it.
    let target = approximating().with_mailboxes([id("finance")]);
    let plan = plan_rules(&imported(), &target, IMPORT);
    let outcomes: Vec<Kept> = plan.outcomes.iter().map(|outcome| outcome.kept).collect();
    assert_eq!(
        outcomes,
        [
            Kept::Disabled,
            Kept::Reduced,
            Kept::Whole,
            Kept::Disabled,
            Kept::Whole
        ]
    );
    let written = plan.written();
    // As an edit plans them, with what the target's own edit knows: not its folders.
    let again = plan_rules(&written, &approximating(), PlanMode::Edit);
    assert_eq!(again.written(), written);
    for outcome in &again.outcomes {
        assert!(
            matches!(outcome.kept, Kept::Whole),
            "rule {} came out {:?}",
            outcome.source,
            outcome.kept
        );
    }
}

#[test]
fn held_forwarding_lists_the_rules_the_switch_decides() {
    let native = NativeAction::new(NativeKind::Graph, "{}");
    let rules = vec![
        // 0: redirects: stored disabled for review.
        MailRule::typed(Match::all(vec![subject("a")]), vec![redirect(1)]),
        // 1: files: imported as it ran.
        MailRule::typed(Match::all(vec![subject("b")]), vec![into("x")]),
        // 2: redirects, but disabled already: the switch changes nothing.
        MailRule::typed(Match::all(vec![subject("c")]), vec![redirect(1)]).disabled(),
        // 3: redirects, but not kept anyway (a condition the target lacks).
        MailRule::typed(
            Match::all(vec![Condition::HasAttachment]),
            vec![redirect(1)],
        ),
        // 4: forwards through a native action the target stores.
        MailRule::typed(Match::default(), vec![Action::Native(native)]),
    ];
    let target = rich().with_native(NativeKind::Graph);
    let held = held_forwarding(&rules, &target);
    let shown: Vec<(usize, Kept)> = held.iter().map(|o| (o.source, o.kept)).collect();
    assert_eq!(shown, [(0, Kept::Disabled), (4, Kept::Disabled)]);
    // Each is the default import's own outcome.
    let plan = plan_rules(&rules, &target, IMPORT);
    assert_eq!(held[0], plan.outcomes[0]);

    // Where the target cannot store a rule disabled, a forwarding rule is not imported at all.
    let no_disabled = without(&rich(), RuleFeature::Disabled);
    let held = held_forwarding(&rules[..2], &no_disabled);
    let shown: Vec<(usize, Kept)> = held.iter().map(|o| (o.source, o.kept)).collect();
    assert_eq!(shown, [(0, Kept::NotKept)]);

    // A carried rule that may send mail cannot be disabled either.
    let opaque = MailRule::opaque(RawRule::new(RuleSyntax::Sieve, "redirect \"a\";").requiring([]));
    let target = rich().with_opaque(RuleSyntax::Sieve, []);
    let held = held_forwarding(std::slice::from_ref(&opaque), &target);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].kept, Kept::NotKept);
}
