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

/// A rule stored with its name left as the server had it is reduced, never dropped: the rule asked
/// for is there.
#[test]
fn a_rule_stored_short_is_reduced() {
    let mut outcome = whole(2);
    outcome.written_short(RulePart::Name);
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(outcome.losses, [not_written(RulePart::Name)]);

    let mut kept_off = whole(2);
    kept_off.kept = Kept::Disabled;
    kept_off.written_short(RulePart::Name);
    assert_eq!(kept_off.kept, Kept::Disabled);
}

/// A rule failed after a part of it was stored short is still not kept: the loss already there
/// must not hide the failure.
#[test]
fn a_rule_not_stored_after_a_short_write_is_not_kept() {
    let mut outcome = whole(0);
    outcome.written_short(RulePart::Rule);
    outcome.not_written(RulePart::Rule);
    assert_eq!(outcome.kept, Kept::NotKept);
    assert_eq!(outcome.losses, [not_written(RulePart::Rule)]);
}

/// The rule asked for is stored, and the one it was read as could not be removed: both run, which
/// is its own reason, not a part the rule lost.
#[test]
fn a_rule_stored_beside_its_old_copy_says_so() {
    let mut outcome = whole(1);
    outcome.left_beside();
    outcome.left_beside();
    assert_eq!(outcome.kept, Kept::Reduced);
    assert_eq!(
        outcome.losses,
        [Loss::new(RulePart::Rule, LossReason::LeftBeside)]
    );
}

#[test]
fn a_receipt_names_the_rules_left_as_read() {
    let receipt = RulesReceipt::new(None, Vec::new());
    assert_eq!(receipt.left_as_read, Vec::<usize>::new());
    assert_eq!(receipt.with_left_as_read(vec![2]).left_as_read, [2]);
}
