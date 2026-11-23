use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    vacation::{AutoReply, ReplyAudience, RulesEffect},
    version::VacationVersion,
};

use super::*;
use crate::{
    CalendarSync, CalendarWrites, Capabilities, ConnectionInfo, MailboxWrites, Provider,
    tests::FakeJmap,
};

fn account() -> AccountId {
    AccountId::try_from("account").unwrap()
}

/// An adapter with an out-of-office reply, so a lost delegation shows: the trait default
/// answers `None`, this answers itself.
struct Replies;

#[async_trait]
impl Provider for Replies {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn vacation(&self) -> Option<&dyn Vacation> {
        Some(self)
    }
}

impl CalendarSync for Replies {}
impl CalendarWrites for Replies {}
impl MailboxWrites for Replies {}

#[async_trait]
impl Vacation for Replies {
    fn vocabulary(&self) -> VacationVocabulary {
        VacationVocabulary::everyone().with_subject()
    }

    fn guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        Ok(VacationResponse::default().with_version(VacationVersion::new("v1")))
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        _base: &VacationResponse,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        refuse_unheld_vacation(&self.vocabulary(), new)?;
        Ok(VacationReceipt::new(
            Some(VacationVersion::new("v2")),
            RulesEffect::Untouched,
        ))
    }
}

/// Reaches the accessor the way a host's code generic over `P: Provider` does, which is the
/// `Box<dyn Provider>` impl's delegation. A plain `boxed.vacation()` would auto-deref to the
/// inner adapter and pass with the forward deleted.
fn vacation_of<P: Provider + ?Sized>(provider: &P) -> Option<&dyn Vacation> {
    provider.vacation()
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_its_reply_through_the_accessor() {
    let held: Box<dyn Provider> = Box::new(Replies);

    let vacation = vacation_of(&held).expect("an adapter with a reply answers itself");

    assert_eq!(vacation.guard(), WriteGuard::Enforced);
    let read = vacation.read_vacation(&account()).await.unwrap();
    let receipt = vacation
        .set_vacation(&account(), &read, &read.clone().turned_on())
        .await
        .unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v2")));
}

#[test]
fn an_adapter_without_a_reply_answers_none_through_the_box_too() {
    let held: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    assert!(vacation_of(&held).is_none());
}

#[test]
fn a_reply_the_account_can_hold_passes() {
    let vocabulary = VacationVocabulary::everyone().with_window().with_subject();
    let reply = VacationResponse::default().turned_on().with_reply(
        AutoReply::default()
            .with_subject("Away")
            .with_text("Back soon."),
    );
    assert!(refuse_unheld_vacation(&vocabulary, &reply).is_ok());
}

#[tokio::test]
async fn a_reply_with_a_part_the_account_cannot_hold_is_refused_naming_the_part_not_its_words() {
    let reply = VacationResponse::default()
        .with_reply(AutoReply::default().with_subject("Secret trip to the clinic"))
        .with_audience(ReplyAudience::Contacts);

    let err = Replies
        .set_vacation(&account(), &VacationResponse::default(), &reply)
        .await
        .expect_err("a reply to contacts only is not written as a reply to everyone");

    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        err.detail().ends_with("cannot hold its audience"),
        "{}",
        err.detail()
    );
    let err = refuse_unheld_vacation(&VacationVocabulary::everyone(), &reply).unwrap_err();
    assert!(
        err.detail().ends_with("its subject, audience"),
        "{}",
        err.detail()
    );
    assert!(!format!("{err:?}").contains("clinic"));
}
