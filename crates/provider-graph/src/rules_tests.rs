//! The rules as the provider offers them: the accessor, the constant vocabulary, the listing's
//! paging, order and version, and how a refusal to read is classified.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    rules::{RuleFeature, RuleLimit, Support, TextOp},
};
use engine_provider::{MailRules, Provider, WriteGuard};
use serde_json::json;

use super::{Listing, vocabulary};
use crate::{
    GraphProvider,
    principal::MailboxPrincipal,
    rules_read::tests::CAPTURED,
    test_support::{FakeRoute, fake_client_logged, json},
};

fn provider(routes: Vec<(&str, FakeRoute)>) -> (GraphProvider, crate::test_support::RequestLog) {
    let (client, _, log) = fake_client_logged(routes);
    let provider = GraphProvider::new(client, MailboxId::try_from("inbox").unwrap());
    (provider, log)
}

fn account() -> AccountId {
    AccountId::try_from("a").unwrap()
}

#[tokio::test]
async fn every_graph_mailbox_offers_its_rules_with_no_server_guard() {
    let (graph, log) = provider(vec![("messageRules", Ok(json(CAPTURED)))]);
    let rules = graph.mail_rules().expect("offered");
    assert_eq!(rules.rules_write_guard(), WriteGuard::Absent);
    assert_eq!(
        rules.rule_vocabulary(&account()).await.unwrap(),
        vocabulary()
    );
    let set = rules.read_rules(&account()).await.unwrap();
    assert_eq!(set.rules.len(), 9);
    assert!(set.version.is_some() && set.raw.is_some());
    let (_, url, _) = &log.lock().unwrap()[0];
    assert_eq!(url, "https://graph.test/me/mailFolders/inbox/messageRules");
}

#[tokio::test]
async fn a_mailbox_named_by_address_reads_its_own_rules() {
    let (client, _, log) = fake_client_logged(vec![("messageRules", Ok(json(CAPTURED)))]);
    let client = client.with_principal(MailboxPrincipal::user("shared@example.test").unwrap());
    Listing::read(&client).await.unwrap();
    let (_, url, _) = &log.lock().unwrap()[0];
    assert_eq!(
        url,
        "https://graph.test/users/shared@example.test/mailFolders/inbox/messageRules"
    );
}

#[test]
fn the_vocabulary_is_the_graph_column() {
    let held = vocabulary();
    for feature in [
        RuleFeature::Except,
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::AddressText(TextOp::Is),
        RuleFeature::FileIntoId,
        RuleFeature::RedirectCopy,
        RuleFeature::Forward,
        RuleFeature::Disabled,
        RuleFeature::Order,
    ] {
        assert_eq!(held.support(feature), Some(Support::Exact), "{feature:?}");
    }
    assert_eq!(
        held.support(RuleFeature::NamedKeyword),
        Some(Support::Approximated)
    );
    for limit in [
        RuleLimit::OneTestPerKind,
        RuleLimit::OneActionPerKind,
        RuleLimit::RecipientsTogether,
        RuleLimit::NoOtherSystemKeyword,
        RuleLimit::RealFoldersOnly,
    ] {
        assert!(held.has_limit(limit), "{limit:?}");
    }
    for feature in [
        RuleFeature::MatchAny,
        // A domain within the sender's text would also match a longer domain: widened.
        RuleFeature::AddressDomain,
        RuleFeature::HeaderText(TextOp::Contains),
        RuleFeature::SubjectText(TextOp::Is),
        RuleFeature::SizeOver,
        RuleFeature::FileInto,
        RuleFeature::Redirect,
        RuleFeature::Flag,
    ] {
        assert_eq!(held.support(feature), None, "{feature:?}");
    }
    assert!(held.stores_native(engine_core::rules::NativeKind::Graph));
}

#[tokio::test]
async fn every_page_is_read_and_the_rules_are_ordered_by_sequence() {
    let mut first = json(CAPTURED);
    let mut rules = first["value"].as_array().unwrap().clone();
    rules.reverse();
    let (late, early) = rules.split_at(4);
    first["value"] = json!(early);
    first["@odata.nextLink"] = json!("https://graph.microsoft.com/v1.0/me/next-page");
    let second = json!({ "value": late });
    let (client, ..) =
        fake_client_logged(vec![("next-page", Ok(second)), ("messageRules", Ok(first))]);
    let listing = Listing::read(&client).await.unwrap();
    let sequences: Vec<i64> = listing
        .rules
        .iter()
        .map(|rule| rule["sequence"].as_i64().unwrap())
        .collect();
    assert_eq!(sequences, (1..=9).collect::<Vec<_>>());
    // The order Graph listed them in does not move the version.
    let (straight, ..) = fake_client_logged(vec![("messageRules", Ok(json(CAPTURED)))]);
    let straight = Listing::read(&straight).await.unwrap();
    assert_eq!(listing.listing.version(), straight.listing.version());
    assert_eq!(
        listing.listing.fingerprint(),
        straight.listing.fingerprint()
    );
}

#[tokio::test]
async fn a_credential_without_the_scope_is_refused_permanently() {
    // A personal account's delegated token without `MailboxSettings.Read` (observed).
    let denied = json!({"error": {"code": "ErrorAccessDenied", "message": "Access is denied."}});
    let (graph, _) = provider(vec![("messageRules", Err((403, denied)))]);
    let err = graph.read_rules(&account()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    let (graph, _) = provider(vec![("messageRules", Ok(json!({"value": 1})))]);
    assert_eq!(
        graph.read_rules(&account()).await.unwrap_err().class(),
        FailureClass::Permanent
    );
}

/// A word match (a search's, Gmail's) has no Graph predicate: the vocabulary does not hold it, so a
/// rule with one is not kept here, and the writer refuses it too.
#[test]
fn a_word_match_is_not_held() {
    use engine_core::rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, Kept, MailRule, Match, PlanMode,
        TextTest, plan_rules,
    };
    let words = TextTest::new(TextOp::Words, vec!["invoice".to_owned()]).unwrap();
    for condition in [
        Condition::Header {
            field: HeaderField::Subject,
            test: words.clone(),
        },
        Condition::Address {
            field: AddressField::From,
            part: AddressPart::WithName,
            test: words,
        },
    ] {
        let rule = MailRule::typed(Match::all(vec![condition]), vec![Action::Discard]);
        let plan = plan_rules(std::slice::from_ref(&rule), &vocabulary(), PlanMode::Edit);
        assert_eq!(plan.outcomes[0].kept, Kept::NotKept);
        assert!(crate::rules_write::body(&rule, &|_| None).is_err());
    }
}
