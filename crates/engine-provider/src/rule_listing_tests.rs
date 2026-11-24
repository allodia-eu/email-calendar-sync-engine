use engine_core::rules::{
    Action, FailedRule, Kept, Match, PlanMode, RuleVocabulary, RuleWriteFailure, plan_rules,
};

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
/// An adapter's parse: the JSON read again and written as it keeps it (here, compact).
fn parse(raw: &RawRules) -> Option<RuleListing> {
    let value: serde_json::Value = serde_json::from_str(raw.as_str()).ok()?;
    Some(RuleListing::new(value.to_string()))
}

#[test]
fn a_base_not_read_from_the_account_is_refused() {
    let listing = listing();
    let read = RuleSet::new(Vec::new())
        .with_version(listing.version())
        .with_raw(listing.raw());
    assert_eq!(RuleListing::of_base(&read, parse, |l| l).unwrap(), listing);
    let unread = [
        RuleSet::new(Vec::new()),
        RuleSet::new(Vec::new()).with_raw(listing.raw()),
        RuleSet::new(Vec::new()).with_version(listing.version()),
        RuleSet::new(Vec::new())
            .with_version(listing.version())
            .with_raw(RawRules::new("[]")),
        RuleSet::new(Vec::new())
            .with_version(listing.version())
            .with_raw(RawRules::new("not json")),
    ];
    for base in unread {
        let err = RuleListing::of_base(&base, parse, |l| l).unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
    }
    assert_eq!(RuleListing::unread().class(), FailureClass::Permanent);
}

/// A host that stored the raw form and handed it back re-encoded (other whitespace, other key
/// order) still names the listing it read: the version is the adapter's parse of it, not its bytes.
#[test]
fn a_base_re_encoded_by_the_host_is_still_the_one_read() {
    let listing = RuleListing::new("[{\"a\":1,\"b\":2}]");
    let base = RuleSet::new(Vec::new())
        .with_version(listing.version())
        .with_raw(RawRules::new("[ { \"b\" : 2 ,\n \"a\" : 1 } ]"));
    assert_eq!(RuleListing::of_base(&base, parse, |l| l).unwrap(), listing);
}

#[test]
fn a_listing_read_now_is_compared_with_the_base_by_version() {
    let listing = listing();
    let base = RuleSet::new(Vec::new())
        .with_version(listing.version())
        .with_raw(listing.raw());
    assert!(listing.unchanged_since(&base).is_ok());
    let moved = RuleListing::new("[]").unchanged_since(&base).unwrap_err();
    assert_eq!(moved.class(), FailureClass::Conflict);
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
    assert_eq!(
        listing.left_as_read(&rules, &outcomes, &[], 2, |_, _| true),
        [0]
    );
    assert_eq!(
        listing.left_as_read(&rules, &outcomes, &[], 2, |_, index| index != 0),
        Vec::<usize>::new()
    );
    // A write that may have landed (a timeout, a gateway's `502` or `504`) may have applied the
    // edit: the rule is not named as left as read. A refusal is the server's own answer, and is.
    let unknown =
        RuleWriteFailure::new(FailedRule::Written(0), FailureClass::Retryable).outcome_unknown();
    let none: Vec<usize> = Vec::new();
    assert_eq!(
        listing.left_as_read(&rules, &outcomes, &[unknown], 2, |_, _| true),
        none
    );
    let refused = RuleWriteFailure::new(FailedRule::Written(0), FailureClass::Permanent);
    assert_eq!(
        listing.left_as_read(&rules, &outcomes, &[refused], 2, |_, _| true),
        [0]
    );
}
