//! What a rule-by-rule write records on a planned outcome: one answer for every adapter that
//! writes rules one at a time.

use super::*;

fn whole(source: usize) -> RuleOutcome {
    RuleOutcome {
        source,
        kept: Kept::Whole,
        losses: Vec::new(),
    }
}

fn not_written(part: RulePart) -> Loss {
    Loss::new(part, LossReason::NotWritten)
}

#[test]
fn a_rule_not_stored_is_not_kept_whatever_the_plan_said() {
    for kept in [Kept::Whole, Kept::Reduced, Kept::Disabled] {
        let mut outcome = whole(0);
        outcome.kept = kept;
        outcome.not_written(RulePart::Rule);
        assert_eq!(outcome.kept, Kept::NotKept);
        assert_eq!(outcome.losses, [not_written(RulePart::Rule)]);
    }
}

#[test]
fn a_part_not_stored_reduces_a_whole_rule_and_leaves_a_lesser_one() {
    let mut outcome = whole(1);
    outcome.not_written(RulePart::Order);
    assert_eq!(outcome.kept, Kept::Reduced);

    let mut disabled = whole(1);
    disabled.kept = Kept::Disabled;
    disabled.not_written(RulePart::Order);
    assert_eq!(disabled.kept, Kept::Disabled);
}

#[test]
fn a_failure_is_recorded_once_per_part() {
    let mut outcome = whole(0);
    outcome.not_written(RulePart::Order);
    outcome.not_written(RulePart::Order);
    assert_eq!(outcome.losses, [not_written(RulePart::Order)]);
}

/// A rule stored with something left as it was (an old copy beside the new one, a name the
/// server kept) is reduced, never dropped: the rule asked for is there.
#[test]
fn a_rule_stored_short_is_reduced_even_on_the_whole_rule() {
    let mut outcome = whole(2);
    outcome.written_short(RulePart::Rule);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(outcome.losses, [not_written(RulePart::Rule)]);

    let mut kept_off = whole(2);
    kept_off.kept = Kept::Disabled;
    kept_off.written_short(RulePart::Name);
    assert_eq!(kept_off.kept, Kept::Disabled);
}
