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

/// The host's map: the source's `src-finance` is the target's `finance`.
fn folders() -> FolderMap {
    FolderMap::new().with(id("src-finance"), id("finance"))
}

#[test]
fn a_folder_id_is_written_as_the_targets_and_one_the_host_did_not_map_is_missing() {
    let target = by_id().with_folders(folders());
    let mapped = MailRule::typed(Match::all(vec![subject("a")]), vec![into_id("src-finance")]);
    let (written, outcome) = one(&mapped, &target, IMPORT);
    assert_eq!(outcome.kept, Kept::Whole);
    let Some(RuleBody::Typed(written)) = written.map(|rule| rule.body) else {
        panic!("the rule is written typed");
    };
    assert_eq!(written.actions, vec![into_id("finance")], "the target's id");

    // An id the host did not map loses its action, even one the target happens to have: ids repeat
    // across accounts, so the source's `finance` is not the target's.
    for unmapped in ["unmapped", "finance"] {
        let rule = MailRule::typed(
            Match::all(vec![subject("a")]),
            vec![into_id(unmapped), mark_read()],
        );
        let (written, outcome) = one(&rule, &target, IMPORT);
        assert_eq!(outcome.kept, Kept::Reduced, "{unmapped}");
        assert_eq!(
            outcome.losses,
            vec![loss(RulePart::Action(0), LossReason::MissingMailbox)]
        );
        let Some(RuleBody::Typed(written)) = written.map(|rule| rule.body) else {
            panic!("the rule is written typed");
        };
        assert_eq!(written.actions, vec![mark_read()]);
    }
    // Filing into it was all it did: nothing is left to do.
    let only = MailRule::typed(Match::all(vec![subject("a")]), vec![into_id("unmapped")]);
    assert_eq!(one(&only, &target, IMPORT).1.kept, Kept::NotKept);
    // Without a map, an id is the account's own, as an edit has it.
    assert_eq!(one(&only, &by_id(), PlanMode::Edit).1.kept, Kept::Whole);
}

#[test]
fn a_missing_mailbox_is_reported_where_the_target_cannot_file_by_id_at_all_too() {
    // The folder has no counterpart: that is the reason, whatever else the target lacks.
    let target = rich().with_folders(folders());
    let rule = MailRule::typed(Match::default(), vec![into_id("unmapped"), mark_read()]);
    assert_eq!(
        one(&rule, &target, IMPORT).1.losses,
        vec![loss(RulePart::Action(0), LossReason::MissingMailbox)]
    );
    // Mapped, it is the target that cannot file by id.
    let rule = MailRule::typed(Match::default(), vec![into_id("src-finance"), mark_read()]);
    assert_eq!(
        one(&rule, &target, IMPORT).1.losses,
        vec![loss(RulePart::Action(0), LossReason::NotExpressible)]
    );
}

#[test]
fn a_folder_map_shows_no_id_in_its_debug() {
    let map = folders().with(id("secret-source"), id("secret-target"));
    assert_eq!(map.target(&id("src-finance")), Some(&id("finance")));
    assert_eq!(map.len(), 2);
    let shown = format!("{map:?} {:?}", by_id().with_folders(map.clone()));
    assert!(
        !shown.contains("secret") && !shown.contains("finance"),
        "{shown}"
    );
    let collected: FolderMap = map.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
    assert_eq!(collected, map);
    assert!(FolderMap::new().is_empty());
}

#[test]
fn an_import_carries_no_rule_whose_text_names_a_folder_by_id_or_another_script() {
    let target = rich().with_opaque(
        RuleSyntax::Sieve,
        ["mailboxid".to_owned(), "include".to_owned()],
    );
    let enabled = PlanMode::Import {
        forwarding: ForwardingRules::Enabled,
    };
    let by_id = MailRule::opaque(
        RawRule::new(RuleSyntax::Sieve, "fileinto :mailboxid \"a\" \"INBOX\";")
            .requiring(["mailboxid".to_owned()])
            .sending_no_mail()
            .including_no_script(),
    );
    let (written, outcome) = one(&by_id, &target, enabled);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::MissingMailbox)]
    );
    let include = MailRule::opaque(
        RawRule::new(RuleSyntax::Sieve, "include :personal \"vacation\";")
            .requiring(["include".to_owned()])
            .naming_no_folder_by_id(),
    );
    let (written, outcome) = one(&include, &target, enabled);
    assert_eq!(written, None);
    assert_eq!(
        outcome.losses,
        vec![loss(RulePart::Rule, LossReason::NotExpressible)]
    );
    // An edit of the account's own carries both.
    assert_eq!(one(&by_id, &target, PlanMode::Edit).1.kept, Kept::Whole);
    assert_eq!(one(&include, &target, PlanMode::Edit).1.kept, Kept::Whole);
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
            vec![
                Action::MarkImportance(Importance::High),
                into_id("src-finance"),
            ],
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
    let target =
        approximating().with_folders(FolderMap::new().with(id("src-finance"), id("finance")));
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

    // A carried rule that may send mail cannot be disabled either: as a reader reads it, naming no
    // folder by id and running no script.
    let opaque = MailRule::opaque(
        RawRule::new(RuleSyntax::Sieve, "redirect \"a\";")
            .requiring([])
            .naming_no_folder_by_id()
            .including_no_script(),
    );
    let target = rich().with_opaque(RuleSyntax::Sieve, []);
    let held = held_forwarding(std::slice::from_ref(&opaque), &target);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].kept, Kept::NotKept);
}

#[test]
fn a_rule_carried_whole_no_reader_vouched_for_is_not_imported_and_is_still_edited() {
    // Built with nothing said of what its text does: assumed to name a folder by id and run a
    // script, as it is assumed to send mail.
    let built = RawRule::new(RuleSyntax::Sieve, "keep;")
        .requiring(Vec::new())
        .sending_no_mail();
    assert!(built.names_folders_by_id() && built.includes_scripts());
    let target = rich().with_opaque(RuleSyntax::Sieve, Vec::new());
    let enabled = PlanMode::Import {
        forwarding: ForwardingRules::Enabled,
    };
    let (written, outcome) = one(&MailRule::opaque(built.clone()), &target, enabled);
    assert_eq!(written, None);
    assert_eq!(outcome.kept, Kept::NotKept);
    // An edit of the account's own rules is not an import: the defaults change nothing there.
    let edit = one(&MailRule::opaque(built.clone()), &target, PlanMode::Edit).1;
    assert_eq!(edit.kept, Kept::Whole);
    // Vouched for, it is carried.
    let read = built.naming_no_folder_by_id().including_no_script();
    assert_eq!(
        one(&MailRule::opaque(read), &target, enabled).1.kept,
        Kept::Whole
    );
}
