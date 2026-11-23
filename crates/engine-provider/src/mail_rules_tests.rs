use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    rules::{RuleSet, RulesReceipt},
    version::RuleSetVersion,
};

use super::*;
use crate::{
    CalendarSync, CalendarWrites, Capabilities, ConnectionInfo, MailboxWrites, Provider,
    tests::FakeJmap,
};

fn account() -> AccountId {
    AccountId::try_from("account").unwrap()
}

/// An adapter with rules, so a lost delegation shows: the trait default answers `None`, this
/// answers itself.
struct Rules;

#[async_trait]
impl Provider for Rules {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn mail_rules(&self) -> Option<&dyn MailRules> {
        Some(self)
    }
}

impl CalendarSync for Rules {}
impl CalendarWrites for Rules {}
impl MailboxWrites for Rules {}

#[async_trait]
impl MailRules for Rules {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(RuleVocabulary::new().with_max_redirects(1))
    }

    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        Ok(RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("r1")))
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        _rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        guard_version(base, self.rules_write_guard())?;
        Ok(RulesReceipt::new(
            Some(RuleSetVersion::new("r2")),
            Vec::new(),
        ))
    }
}

/// Reaches the accessor as a host generic over `P: Provider` does, through the `Box` impl's
/// delegation; a plain `boxed.mail_rules()` would auto-deref past it.
fn rules_of<P: Provider + ?Sized>(provider: &P) -> Option<&dyn MailRules> {
    provider.mail_rules()
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_its_rules_through_the_accessor() {
    let held: Box<dyn Provider> = Box::new(Rules);
    let rules = rules_of(&held).expect("an adapter with rules answers itself");
    assert_eq!(rules.rules_write_guard(), WriteGuard::Enforced);
    assert_eq!(
        rules
            .rule_vocabulary(&account())
            .await
            .unwrap()
            .max_redirects(),
        Some(1)
    );
    let read = rules.read_rules(&account()).await.unwrap();
    let receipt = rules.replace_rules(&account(), &read, &[]).await.unwrap();
    assert_eq!(receipt.version, Some(RuleSetVersion::new("r2")));
}

#[test]
fn an_adapter_without_rules_answers_none_through_the_box_too() {
    let held: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    assert!(rules_of(&held).is_none());
}

#[test]
fn the_guard_is_the_read_version_on_an_enforcing_transport_and_refused_without_one() {
    let read = RuleSet::new(Vec::new()).with_version(RuleSetVersion::new("s1"));
    assert_eq!(
        guard_version(&read, WriteGuard::Enforced)
            .unwrap()
            .map(RuleSetVersion::as_str),
        Some("s1")
    );
    assert_eq!(guard_version(&read, WriteGuard::Absent).unwrap(), None);
    let built = RuleSet::new(Vec::new());
    assert_eq!(
        guard_version(&built, WriteGuard::Enforced)
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
    assert_eq!(guard_version(&built, WriteGuard::Absent).unwrap(), None);
}
