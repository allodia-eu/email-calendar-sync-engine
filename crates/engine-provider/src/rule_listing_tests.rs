use engine_core::rules::{Action, Kept, Match, PlanMode, RuleVocabulary, plan_rules};

use super::*;

fn listing() -> RuleListing {
    RuleListing::new("[{\"id\":\"a\"},{\"id\":\"b\"}]")
}

fn rule(origin: Option<RuleOrigin>) -> MailRule {
    let rule = MailRule::typed(Match::all(Vec::new()), vec![Action::Discard]);
    match origin {
        Some(origin) => rule.with_origin(origin),
        None => rule,
    }
}

#[test]
fn a_listing_names_its_version_and_its_rules_by_position() {
    let listing = listing();
    assert_eq!(listing.version().as_str().len(), 64);
    assert_eq!(
        listing.version(),
        RuleListing::new(listing.text()).version()
    );
    assert_ne!(listing.version(), RuleListing::new("[]").version());
    let origin = listing.origin(1);
    assert_eq!(listing.index_of(&rule(Some(origin)), 2), Some(1));
    // Read from another listing, past the end of this one, or from no listing: none of its own.
    let other = RuleListing::new("[]").origin(1);
    assert_eq!(listing.index_of(&rule(Some(other)), 2), None);
    assert_eq!(listing.index_of(&rule(Some(listing.origin(2))), 2), None);
    assert_eq!(listing.index_of(&rule(None), 2), None);
    assert_eq!(listing.raw().as_str(), listing.text());
    assert_eq!(
        digest_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// Without its version and raw form a write could not tell which rules to keep, and would remove
/// every rule unseen; with a raw form its version does not name, it was not read as it says.
#[test]
fn a_base_not_read_from_the_account_is_refused() {
    let listing = listing();
    let read = RuleSet::new(Vec::new())
        .with_version(listing.version())
        .with_raw(listing.raw());
    assert_eq!(
        RuleListing::base_raw(&read).unwrap().as_str(),
        listing.text()
    );
    let unread = [
        RuleSet::new(Vec::new()),
        RuleSet::new(Vec::new()).with_raw(listing.raw()),
        RuleSet::new(Vec::new()).with_version(listing.version()),
        RuleSet::new(Vec::new())
            .with_version(listing.version())
            .with_raw(RawRules::new("[]")),
    ];
    for base in unread {
        let err = RuleListing::base_raw(&base).unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
    }
    assert_eq!(RuleListing::unread().class(), FailureClass::Permanent);
}

#[test]
fn a_rule_not_kept_that_names_a_rule_still_there_is_left_as_read() {
    let listing = listing();
    let mut disabled = rule(Some(listing.origin(0))).disabled();
    disabled.name = None;
    let rules = [
        disabled,
        rule(Some(listing.origin(1))),
        rule(None).disabled(),
    ];
    // A target that cannot store a rule off keeps neither disabled one.
    let target = RuleVocabulary::new().with_exact([engine_core::rules::RuleFeature::Discard]);
    let outcomes = plan_rules(&rules, &target, PlanMode::Edit).outcomes;
    assert_eq!(outcomes[0].kept, Kept::NotKept);
    assert_eq!(listing.left_as_read(&rules, &outcomes, 2, |_, _| true), [0]);
    assert_eq!(
        listing.left_as_read(&rules, &outcomes, 2, |_, index| index != 0),
        Vec::<usize>::new()
    );
}
