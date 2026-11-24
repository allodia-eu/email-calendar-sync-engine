//! The pure checks the live rules suite makes before and after each write: whether two rules say
//! the same, whether the engine would write the mailbox's own rules back whole, and finding the
//! test's own rule by its tag. Tested offline, so each guard is seen to fail.

use engine_core::{
    mail::{Keyword, SystemKeyword},
    rules::{
        Action, Condition, HeaderField, Kept, MailRule, Match, PlanMode, RuleBody, RuleFeature,
        RuleName, RuleVocabulary, TextOp, TextTest, plan_rules,
    },
};

/// Whether two rules say the same, whatever order their conditions and actions are in: Graph
/// keeps each as a key of one object, and the reader lists them in key order.
pub(crate) fn same(a: &MailRule, b: &MailRule) -> bool {
    let (RuleBody::Typed(x), RuleBody::Typed(y)) = (&a.body, &b.body) else {
        return a == b;
    };
    a.name == b.name
        && a.enabled == b.enabled
        && x.stop == y.stop
        && x.when.mode == y.when.mode
        && multiset(&x.when.conditions, &y.when.conditions)
        && multiset(&x.when.except, &y.when.except)
        && multiset(&x.actions, &y.actions)
}

/// Whether two lists hold the same items as often each, in any order.
pub(crate) fn multiset<T: PartialEq>(p: &[T], q: &[T]) -> bool {
    let count = |list: &[T], item: &T| list.iter().filter(|other| *other == item).count();
    p.len() == q.len() && p.iter().all(|item| count(p, item) == count(q, item))
}

/// How many of `rules` the engine would not write back whole against `vocabulary`.
pub(crate) fn not_whole(rules: &[MailRule], vocabulary: &RuleVocabulary) -> usize {
    plan_rules(rules, vocabulary, PlanMode::Edit)
        .outcomes
        .iter()
        .filter(|outcome| outcome.kept != Kept::Whole)
        .count()
}

/// The index of this run's rule tagged `tag`, or a stop: an edit never names a rule by its index.
pub(crate) fn index_of(rules: &[MailRule], marker: &str, tag: &str) -> usize {
    let wanted = format!("{marker}-{tag}");
    rules
        .iter()
        .position(|rule| {
            let RuleBody::Typed(typed) = &rule.body else {
                return false;
            };
            typed.when.conditions.iter().any(|condition| {
                matches!(condition, Condition::Header { test, .. }
                    if test.values().contains(&wanted))
            })
        })
        .unwrap_or_else(|| panic!("no rule of this run is tagged {tag}; stopping"))
}

/// A rule of the run tagged `tag`, marking mail read.
fn rule(marker: &str, tag: &str, actions: Vec<Action>) -> MailRule {
    MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec![format!("{marker}-{tag}")]).unwrap(),
        }]),
        actions,
    )
    .named(RuleName::new(format!("Engine test {tag} {marker}")).unwrap())
}

fn mark_read() -> Action {
    Action::AddKeywords([Keyword::system(SystemKeyword::Seen)].into())
}

#[test]
fn rules_compare_as_multisets_in_any_order() {
    let rule = |actions: Vec<Action>| rule("m", "t", actions);
    let read = mark_read();
    let flag = Action::AddKeywords([Keyword::system(SystemKeyword::Flagged)].into());
    assert!(same(
        &rule(vec![read.clone(), flag.clone()]),
        &rule(vec![flag.clone(), read.clone()])
    ));
    assert!(!same(
        &rule(vec![read.clone(), read.clone(), flag.clone()]),
        &rule(vec![read.clone(), flag.clone(), flag.clone()])
    ));
}

#[test]
fn the_guard_counts_existing_rules_graph_would_not_write_back_whole() {
    let vocabulary = RuleVocabulary::new().with_exact([
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::MarkRead,
        RuleFeature::Name,
        RuleFeature::Order,
    ]);
    let held = rule("m", "a", vec![mark_read()]);
    let mut moved_too = held.clone();
    if let RuleBody::Typed(typed) = &mut moved_too.body {
        typed.actions.push(Action::Discard);
    }
    assert_eq!(not_whole(std::slice::from_ref(&held), &vocabulary), 0);
    assert_eq!(not_whole(&[held, moved_too], &vocabulary), 1);
}

#[test]
#[should_panic(expected = "no rule of this run is tagged")]
fn an_edit_never_names_a_rule_that_is_not_this_runs() {
    let theirs = rule("someone-else", "e", vec![mark_read()]);
    index_of(&[theirs], "m", "e");
}

#[test]
fn an_edit_finds_this_runs_rule_by_its_tag() {
    let rules = [
        rule("m", "a", vec![mark_read()]),
        rule("m", "b", vec![mark_read()]),
    ];
    assert_eq!(index_of(&rules, "m", "b"), 1);
}
