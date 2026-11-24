use std::sync::Mutex;

use engine_core::{
    error::FailureClass,
    rules::{
        Action, Condition, HeaderField, Kept, LossReason, MailboxPath, Match, RuleAddress,
        RuleBody, RuleFeature, RuleMailbox, RuleName, RulePart, RuleVocabulary, RulesActivation,
        TextOp, TextTest,
    },
    vacation::{AutoReply, ReplyAudience, VacationVocabulary, VacationWindow, WindowSupport},
    version::{RuleSetVersion, VacationVersion},
};

use super::*;
use crate::{
    CalendarSync, CalendarWrites, Capabilities, ConnectionInfo, MailboxWrites, Provider,
    WriteGuard, tests::FakeJmap,
};

fn account() -> AccountId {
    AccountId::try_from("account").unwrap()
}

fn subject(value: &str) -> Condition {
    Condition::Header {
        field: HeaderField::Subject,
        test: TextTest::new(TextOp::Contains, vec![value.to_owned()]).unwrap(),
    }
}

fn into(path: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Path(MailboxPath::new(path).unwrap()),
        copy: false,
    }
}

fn into_id(id: &str) -> Action {
    Action::FileInto {
        mailbox: RuleMailbox::Id(MailboxId::try_from(id).unwrap()),
        copy: false,
    }
}

fn redirect() -> Action {
    Action::Redirect {
        to: vec![RuleAddress::new("away@example.com").unwrap()],
        copy: false,
    }
}

fn rule(name: &str, actions: Vec<Action>) -> MailRule {
    MailRule::typed(Match::all(vec![subject(name)]), actions).named(RuleName::new(name).unwrap())
}

fn vocabulary() -> RuleVocabulary {
    RuleVocabulary::new().with_exact([
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::FileInto,
        RuleFeature::FileIntoId,
        RuleFeature::Redirect,
        RuleFeature::Disabled,
        RuleFeature::Name,
        RuleFeature::Order,
    ])
}

/// A target with rules and a reply, recording what it was asked to write.
struct Target {
    vocabulary: RuleVocabulary,
    own: RuleSet,
    replaced: Mutex<Vec<(RuleSet, Vec<MailRule>)>>,
    refuse_replace: Option<FailureClass>,
    reply: VacationResponse,
    replies: Mutex<Vec<(Option<VacationVersion>, VacationResponse)>>,
}

impl Target {
    fn with_own(own: Vec<MailRule>) -> Self {
        Self {
            vocabulary: vocabulary(),
            own: RuleSet::new(own).with_version(RuleSetVersion::new("r1")),
            replaced: Mutex::new(Vec::new()),
            refuse_replace: None,
            reply: VacationResponse::default().with_version(VacationVersion::new("v1")),
            replies: Mutex::new(Vec::new()),
        }
    }

    fn replaced(&self) -> Vec<(RuleSet, Vec<MailRule>)> {
        self.replaced.lock().unwrap().clone()
    }
}

#[async_trait]
impl MailRules for Target {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(self.vocabulary.clone())
    }

    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        Ok(self.own.clone())
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        self.replaced
            .lock()
            .unwrap()
            .push((base.clone(), rules.to_vec()));
        if let Some(class) = self.refuse_replace {
            return Err(ProviderError::new(class, "refused"));
        }
        let edit = plan_rules(rules, &self.vocabulary, PlanMode::Edit);
        Ok(
            RulesReceipt::new(Some(RuleSetVersion::new("r2")), edit.outcomes)
                .with_activation(self.own.activation),
        )
    }
}

#[async_trait]
impl Vacation for Target {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        Ok(VacationVocabulary::everyone()
            .with_subject()
            .with_window(WindowSupport::EitherEnd))
    }

    fn vacation_write_guard(&self) -> WriteGuard {
        WriteGuard::Enforced
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        Ok(self.reply.clone())
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        let guard = precondition.version(self.vacation_write_guard())?.cloned();
        self.replies.lock().unwrap().push((guard, new.clone()));
        Ok(VacationReceipt::new(
            Some(VacationVersion::new("v2")),
            RulesEffect::Untouched,
        ))
    }
}

#[async_trait]
impl RulesImporter for Target {
    async fn import_rules(
        &self,
        account: &AccountId,
        rules: &[MailRule],
        forwarding: ForwardingRules,
    ) -> ProviderResult<RulesReceipt> {
        let mailboxes = [MailboxId::try_from("finance").unwrap()].into();
        append_rules(self, account, rules, forwarding, &mailboxes).await
    }

    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        import_reply(self, account, reply).await
    }
}

#[async_trait]
impl Provider for Target {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }

    fn rules_importer(&self) -> Option<&dyn RulesImporter> {
        Some(self)
    }
}

impl CalendarSync for Target {}
impl CalendarWrites for Target {}
impl MailboxWrites for Target {}

#[tokio::test]
async fn imported_rules_are_appended_after_the_targets_own_as_they_were_read() {
    let own = vec![rule("mine", vec![into("Mine")]), rule("also", vec![])];
    let target = Target::with_own(own.clone());
    let imported = vec![
        rule("forward", vec![redirect()]),
        rule("file", vec![into_id("unmapped"), into("Kept")]),
        rule("finance", vec![into_id("finance")]),
    ];
    let receipt = target
        .import_rules(&account(), &imported, ForwardingRules::Disabled)
        .await
        .unwrap();

    let replaced = target.replaced();
    assert_eq!(replaced.len(), 1, "one write");
    let (base, written) = &replaced[0];
    // Built on the read, so it is guarded as an edit is.
    assert_eq!(base, &target.own);
    assert_eq!(&written[..2], &own[..]);
    assert_eq!(written.len(), 5);
    assert!(!written[2].enabled, "the redirect arrives disabled");
    let RuleBody::Typed(file) = &written[3].body else {
        panic!("typed");
    };
    assert_eq!(file.actions, vec![into("Kept")]);

    // Indexed by the rules handed in, as the import planned them.
    let kept: Vec<(usize, Kept)> = receipt
        .outcomes
        .iter()
        .map(|o| (o.source, o.kept))
        .collect();
    assert_eq!(
        kept,
        [(0, Kept::Disabled), (1, Kept::Reduced), (2, Kept::Whole)]
    );
    let losses: Vec<(RulePart, LossReason)> = receipt.outcomes[1]
        .losses
        .iter()
        .map(|loss| (loss.part, loss.reason))
        .collect();
    assert_eq!(losses, [(RulePart::Action(0), LossReason::MissingMailbox)]);
    assert_eq!(receipt.version, Some(RuleSetVersion::new("r2")));
}

#[tokio::test]
async fn forwarding_armed_by_the_host_arrives_running() {
    let target = Target::with_own(Vec::new());
    let receipt = target
        .import_rules(
            &account(),
            &[rule("forward", vec![redirect()])],
            ForwardingRules::Enabled,
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::Whole);
    assert!(target.replaced()[0].1[0].enabled);
}

#[tokio::test]
async fn a_target_whose_own_rules_an_edit_would_change_takes_no_import() {
    // The target holds a rule its own edit would not write back: one that names a folder by
    // role, which this vocabulary cannot.
    let odd = MailRule::typed(
        Match::all(vec![subject("x")]),
        vec![Action::FileInto {
            mailbox: RuleMailbox::Role(engine_core::mail::MailboxRole::Trash),
            copy: false,
        }],
    );
    let target = Target::with_own(vec![odd]);
    let err = target
        .import_rules(
            &account(),
            &[rule("new", vec![into("x")])],
            ForwardingRules::Disabled,
        )
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(target.replaced().is_empty(), "nothing written");

    // A rule the edit would keep, but reduced, refuses it too.
    let reduced = rule("y", vec![into("x"), Action::Discard]);
    let target = Target::with_own(vec![reduced]);
    let err = target
        .import_rules(&account(), &[], ForwardingRules::Disabled)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable());
}

#[tokio::test]
async fn nothing_left_to_add_writes_nothing() {
    let mut target = Target::with_own(vec![rule("mine", vec![])]);
    target.own.activation = RulesActivation::Inactive;
    let receipt = target
        .import_rules(
            &account(),
            &[rule("gone", vec![into_id("unmapped")])],
            ForwardingRules::Disabled,
        )
        .await
        .unwrap();
    assert!(target.replaced().is_empty(), "nothing written");
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert_eq!(receipt.version, Some(RuleSetVersion::new("r1")));
    assert_eq!(receipt.stored, RulesStored::InPlace);
    assert_eq!(receipt.activation, RulesActivation::Inactive);
}

#[tokio::test]
async fn a_failed_write_is_the_targets_own_answer() {
    let mut target = Target::with_own(Vec::new());
    target.refuse_replace = Some(FailureClass::Conflict);
    let err = target
        .import_rules(
            &account(),
            &[rule("new", vec![into("x")])],
            ForwardingRules::Disabled,
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

fn away() -> VacationResponse {
    VacationResponse::default().turned_on().with_reply(
        AutoReply::default()
            .with_subject("Away")
            .with_text("Back soon"),
    )
}

#[tokio::test]
async fn a_reply_is_imported_where_the_target_holds_none() {
    let target = Target::with_own(Vec::new());
    let receipt = target.import_vacation(&account(), &away()).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v2")));
    let replies = target.replies.lock().unwrap().clone();
    assert_eq!(replies, [(Some(VacationVersion::new("v1")), away())]);
}

#[tokio::test]
async fn the_same_reply_again_writes_nothing() {
    let mut target = Target::with_own(Vec::new());
    target.reply = away().with_version(VacationVersion::new("v7"));
    let receipt = target.import_vacation(&account(), &away()).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v7")));
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert!(target.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_reply_of_the_targets_own_is_never_replaced() {
    // On, or off with words of its own: either is the target's.
    for held in [
        away().with_reply(AutoReply::default().with_text("Other")),
        away().turned_off(),
        VacationResponse::default().with_window(VacationWindow::default().with_start(
            engine_core::time::UtcDateTime::parse_rfc3339("2026-01-01T00:00:00Z").unwrap(),
        )),
        VacationResponse::default().with_audience(ReplyAudience::Contacts),
    ] {
        let mut target = Target::with_own(Vec::new());
        target.reply = held;
        let err = target
            .import_vacation(&account(), &away())
            .await
            .unwrap_err();
        assert!(err.is_item_unavailable(), "{err:?}");
        assert!(target.replies.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_reply_with_a_part_the_target_cannot_hold_is_refused_whole() {
    let target = Target::with_own(Vec::new());
    let html = away().with_outside_reply(AutoReply::default().with_text("Outside"));
    let err = target.import_vacation(&account(), &html).await.unwrap_err();
    assert!(err.is_item_unavailable());
    assert!(target.replies.lock().unwrap().is_empty());
}

/// Reaches the accessor as a host generic over `P: Provider` does, through the `Box` impl.
fn importer_of<P: Provider + ?Sized>(provider: &P) -> Option<&dyn RulesImporter> {
    provider.rules_importer()
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_its_rules_importer() {
    let held: Box<dyn Provider> = Box::new(Target::with_own(Vec::new()));
    let importer = importer_of(&held).expect("an adapter that imports rules answers itself");
    let receipt = importer.import_vacation(&account(), &away()).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v2")));

    let none: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    assert!(importer_of(&none).is_none());
}
