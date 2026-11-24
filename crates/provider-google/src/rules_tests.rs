use engine_core::{
    mail::{Keyword, MailboxRole},
    rules::{
        Action, AddressField, AddressPart, Condition, Kept, Loss, LossReason, Match, PlanMode,
        RuleMailbox, RuleName, RulePart, TextOp, TextTest, plan_rules,
    },
};
use serde_json::json;

use super::*;
use crate::rules_read::tests::{from, named, subject};

fn listing() -> Listing {
    Listing::new(vec![
        json!({"id": "b", "criteria": {"from": "a@example.com"}, "action": {"addLabelIds": ["STARRED"]}}),
        json!({"id": "a", "criteria": {"subject": "x"}, "action": {"addLabelIds": ["TRASH"]}}),
    ])
}

#[test]
fn the_listing_is_kept_in_id_order_so_its_version_ignores_how_gmail_listed_it() {
    let one = listing();
    let mut reversed = one.filters.clone();
    reversed.reverse();
    let other = Listing::new(reversed);
    assert_eq!(one.version(), other.version());
    assert_eq!(one.set(), other.set());
    assert_eq!(opt_str(&one.filters[0], "id"), Some("a"));
    assert_eq!(one.version().len(), 64);

    let set = one.rule_set(&named());
    let back = Listing::from_raw(set.raw.as_ref().unwrap()).unwrap();
    assert_eq!(back.version(), set.version.unwrap().as_str());
    assert!(Listing::from_raw(&RawRules::new("{}")).is_none());
    assert!(Listing::from_raw(&RawRules::new("not json")).is_none());
}

/// A set read from Gmail is written back as it is: every rule plans whole, but for the order
/// Gmail does not keep.
#[test]
fn rules_read_from_gmail_plan_whole_against_its_own_vocabulary() {
    let filters = vec![
        json!({"id": "1", "criteria": {"from": "a OR b", "to": "c", "subject": "\"d e\"",
            "hasAttachment": true, "size": 5, "sizeComparison": "smaller",
            "negatedQuery": "from:x OR larger:9"},
            "action": {"addLabelIds": ["STARRED", "IMPORTANT", "Label_77"],
                "removeLabelIds": ["INBOX", "UNREAD"], "forward": "f@example.net"}}),
        json!({"id": "2", "criteria": {"query": "from:a OR subject:b", "excludeChats": true},
            "action": {"addLabelIds": ["SPAM", "CATEGORY_SOCIAL"], "removeLabelIds": ["IMPORTANT"]}}),
        json!({"id": "3", "criteria": {"query": "invoice"}, "action": {"removeLabelIds": ["INBOX"]}}),
    ];
    let set = Listing::new(filters).rule_set(&named());
    let plan = plan_rules(&set.rules, &vocabulary(true), PlanMode::Edit);
    assert_eq!(plan.written(), set.rules);
    for outcome in &plan.outcomes {
        assert_eq!(outcome.kept, Kept::Reduced, "{outcome:?}");
        assert!(
            outcome
                .losses
                .iter()
                .all(|loss| loss.reason == LossReason::Approximated
                    || *loss == Loss::new(RulePart::Order, LossReason::NotExpressible)),
            "{outcome:?}"
        );
    }
    // A keyword kept as a label is held only where the host named one.
    let unnamed = plan_rules(&set.rules[..1], &vocabulary(false), PlanMode::Edit);
    assert_eq!(unnamed.outcomes[0].kept, Kept::Reduced);
    assert!(
        unnamed.outcomes[0]
            .losses
            .contains(&Loss::new(RulePart::Action(1), LossReason::NotExpressible))
    );
}

#[test]
fn what_a_filter_cannot_be_is_decided_by_the_plan() {
    let vocabulary = vocabulary(false);
    let trash = vec![Action::Discard];
    let plan = |rule: MailRule| {
        plan_rules(&[rule], &vocabulary, PlanMode::Edit)
            .outcomes
            .remove(0)
    };

    // Stored off: Gmail runs every filter it holds.
    assert_eq!(
        plan(MailRule::typed(Match::all(vec![subject(&["x"])]), trash.clone()).disabled()).kept,
        Kept::NotKept
    );
    // A name and a stop are lost; the rule does less, never more.
    let named = MailRule::typed(Match::all(vec![subject(&["x"])]), trash.clone())
        .named(RuleName::new("n").unwrap())
        .stopping();
    assert_eq!(plan(named).kept, Kept::Reduced);
    // A condition Gmail would widen drops the rule.
    let widened = [
        Condition::Header {
            field: engine_core::rules::HeaderField::Subject,
            test: TextTest::new(TextOp::Is, vec!["x".to_owned()]).unwrap(),
        },
        Condition::Body(TextTest::new(TextOp::Contains, vec!["x".to_owned()]).unwrap()),
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::Domain,
            test: TextTest::new(TextOp::Is, vec!["example.com".to_owned()]).unwrap(),
        },
        Condition::Address {
            field: AddressField::To,
            part: AddressPart::All,
            test: TextTest::new(TextOp::Contains, vec!["x".to_owned()]).unwrap(),
        },
        Condition::SentToMe(engine_core::rules::MeField::To),
        Condition::Importance(engine_core::mail::Importance::High),
    ];
    for condition in widened {
        let rule = MailRule::typed(Match::all(vec![condition.clone()]), trash.clone());
        assert_eq!(plan(rule).kept, Kept::NotKept, "{condition:?}");
        let except = MailRule::typed(
            Match::all(vec![from(&["a"])]).except(vec![condition.clone()]),
            trash.clone(),
        );
        assert_eq!(plan(except).kept, Kept::NotKept, "{condition:?}");
    }
    // One forward address per filter; a redirect that keeps no copy is no Gmail forward.
    let two = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![Action::Redirect {
            to: vec![
                engine_core::rules::RuleAddress::new("a@example.com").unwrap(),
                engine_core::rules::RuleAddress::new("b@example.com").unwrap(),
            ],
            copy: true,
        }],
    );
    let outcome = plan(two);
    assert_eq!(outcome.kept, Kept::NotKept);
    assert!(outcome.losses.contains(&Loss::new(
        RulePart::Action(0),
        LossReason::ForwardNotAllowed
    )));
    let without_copy = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![
            Action::Redirect {
                to: vec![engine_core::rules::RuleAddress::new("a@example.com").unwrap()],
                copy: false,
            },
            Action::Discard,
        ],
    );
    assert_eq!(plan(without_copy).kept, Kept::Reduced);
    // Only real folders, and no system keyword but read and flagged.
    let all_mail = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(MailboxRole::All),
            copy: false,
        }],
    );
    assert_eq!(plan(all_mail).kept, Kept::NotKept);
    let answered = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![Action::AddKeywords(
            [Keyword::system(engine_core::mail::SystemKeyword::Answered)].into(),
        )],
    );
    assert_eq!(plan(answered).kept, Kept::NotKept);
}

/// Gmail answers an account with no filters `204 No Content`, with no body at all (observed live;
/// `forwardingAddresses` does the same): that is an empty listing, not a malformed one.
#[tokio::test]
async fn no_filters_is_an_empty_answer_and_an_empty_listing() {
    let (base, _seen) = crate::test_support::capturing_server("204 No Content", "");
    let client = GoogleClient::with_base(
        "t",
        base,
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    let listing = Listing::read(&client).await.unwrap();
    assert_eq!(listing.filters.len(), 0);
    assert_eq!(listing.rule_set(&named()).rules.len(), 0);
}

#[test]
fn one_label_of_the_users_per_filter_and_none_without_a_criterion() {
    let plan = |rule: MailRule| {
        plan_rules(&[rule], &vocabulary(true), PlanMode::Edit)
            .outcomes
            .remove(0)
    };
    let label = |id: &str| Action::FileInto {
        mailbox: RuleMailbox::Id(engine_core::ids::MailboxId::try_from(id).unwrap()),
        copy: true,
    };
    let two = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![label("Label_1"), label("Label_2")],
    );
    assert!(
        plan(two)
            .losses
            .contains(&Loss::new(RulePart::Action(1), LossReason::NotExpressible))
    );
    let keyword = MailRule::typed(
        Match::all(vec![from(&["a"])]),
        vec![
            label("Label_1"),
            Action::AddKeywords([Keyword::new("$work").unwrap()].into()),
        ],
    );
    assert!(
        plan(keyword)
            .losses
            .contains(&Loss::new(RulePart::Action(1), LossReason::NotExpressible))
    );
    let every = MailRule::typed(Match::all(Vec::new()), vec![Action::Discard]);
    assert_eq!(plan(every).kept, Kept::NotKept);
}
