use std::sync::Mutex;

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    rules::{
        Action, Condition, FolderMap, HeaderField, Kept, LossReason, MailboxPath, Match,
        RuleAddress, RuleBody, RuleFeature, RuleMailbox, RuleName, RulePart, RuleVocabulary,
        RulesActivation, TextOp, TextTest,
    },
    vacation::{AutoReply, RulesEffect, VacationVocabulary, WindowSupport},
    version::{RuleSetVersion, VacationVersion},
};

use super::*;
use crate::{
    CalendarSync, CalendarWrites, Capabilities, ConnectionInfo, MailboxWrites, Provider, Vacation,
    VacationPrecondition, WriteGuard, tests::FakeJmap,
};

pub(crate) fn account() -> AccountId {
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

/// The host's folder map: the source's `src-finance` is the target's `finance`.
fn folders() -> FolderMap {
    FolderMap::new().with(
        MailboxId::try_from("src-finance").unwrap(),
        MailboxId::try_from("finance").unwrap(),
    )
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
pub(crate) struct Target {
    pub(crate) vocabulary: RuleVocabulary,
    pub(crate) own: RuleSet,
    pub(crate) replaced: Mutex<Vec<(RuleSet, Vec<MailRule>)>>,
    pub(crate) refuse_replace: Option<FailureClass>,
    pub(crate) reply: VacationResponse,
    pub(crate) replies: Mutex<Vec<(Option<VacationVersion>, VacationResponse)>>,
    pub(crate) dialect: ImportDialect,
}

impl Target {
    pub(crate) fn with_own(own: Vec<MailRule>) -> Self {
        Self {
            vocabulary: vocabulary(),
            own: RuleSet::new(own).with_version(RuleSetVersion::new("r1")),
            replaced: Mutex::new(Vec::new()),
            refuse_replace: None,
            reply: VacationResponse::default().with_version(VacationVersion::new("v1")),
            replies: Mutex::new(Vec::new()),
            dialect: ImportDialect::as_given(),
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
        folders: &FolderMap,
    ) -> ProviderResult<RulesReceipt> {
        append_rules(self, account, rules, forwarding, folders, self.dialect).await
    }

    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        super::super::import_reply(self, account, reply).await
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
        rule("finance", vec![into_id("src-finance")]),
    ];
    let receipt = target
        .import_rules(&account(), &imported, ForwardingRules::Disabled, &folders())
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
            &folders(),
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
            &folders(),
        )
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(target.replaced().is_empty(), "nothing written");

    // A rule the edit would keep, but reduced, refuses it too.
    let reduced = rule("y", vec![into("x"), Action::Discard]);
    let target = Target::with_own(vec![reduced]);
    let err = target
        .import_rules(&account(), &[], ForwardingRules::Disabled, &folders())
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
            &folders(),
        )
        .await
        .unwrap();
    assert!(target.replaced().is_empty(), "nothing written");
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert_eq!(receipt.version, Some(RuleSetVersion::new("r1")));
    assert_eq!(receipt.stored, RulesStored::Unwritten);
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
            &folders(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn imported_rules_are_written_as_rules_of_no_script() {
    // Read from a source script, a rule names that script's segment; a target script with the
    // same text would otherwise take it for one of its own.
    let origin = engine_core::rules::RuleOrigin::ScriptSegment {
        script: 7,
        index: 1,
    };
    let target = Target::with_own(Vec::new());
    let read = rule("from source", vec![into("x")]).with_origin(origin);
    let _ = target
        .import_rules(&account(), &[read], ForwardingRules::Disabled, &folders())
        .await
        .unwrap();
    assert_eq!(target.replaced()[0].1[0].origin, None);
}

#[tokio::test]
async fn a_rule_the_target_already_holds_is_not_written_again() {
    let disabled_forward = rule("forward", vec![redirect()]).disabled();
    let own = vec![
        rule("same", vec![into("x"), into("z")]),
        // As a first import left it: disabled for review.
        disabled_forward,
    ];
    let target = Target::with_own(own.clone());
    let imported = vec![
        rule("same", vec![into("x"), into("z")]),
        rule("same", vec![into("x"), into("z")]),
        rule("forward", vec![redirect()]),
        rule("new", vec![into("y")]),
        // Not the same: one action twice is not two different ones.
        rule("same", vec![into("x"), into("x")]),
    ];
    let receipt = target
        .import_rules(&account(), &imported, ForwardingRules::Disabled, &folders())
        .await
        .unwrap();
    // One held `same` answers for one of the two handed in; the forward is held as planned.
    assert_eq!(receipt.already_held, [0, 2]);
    let written = &target.replaced()[0].1;
    assert_eq!(&written[..2], &own[..]);
    assert_eq!(written[2..].len(), 3, "{written:?}");
    assert_eq!(written[2].name, imported[1].name);
    assert_eq!(written[3].name, imported[3].name);
    assert_eq!(
        written[4], imported[4],
        "one action twice is a rule of its own"
    );
    assert_eq!(
        receipt.outcomes.len(),
        5,
        "every rule handed in has its outcome"
    );

    // Run again over what that wrote: nothing is written.
    let again = Target::with_own(written.clone());
    let receipt = again
        .import_rules(&account(), &imported, ForwardingRules::Disabled, &folders())
        .await
        .unwrap();
    assert_eq!(receipt.already_held, [0, 1, 2, 3, 4]);
    assert_eq!(receipt.stored, RulesStored::Unwritten);
    assert!(again.replaced().is_empty(), "nothing written");
}

#[tokio::test]
async fn a_rule_carried_whole_is_never_taken_as_already_held() {
    let raw = engine_core::rules::RawRule::new(engine_core::rules::RuleSyntax::Sieve, "keep;")
        .requiring(Vec::new())
        .sending_no_mail();
    let mut target = Target::with_own(vec![MailRule::opaque(raw.clone())]);
    target.vocabulary = vocabulary().with_opaque(engine_core::rules::RuleSyntax::Sieve, Vec::new());
    let receipt = target
        .import_rules(
            &account(),
            &[MailRule::opaque(raw)],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert!(receipt.already_held.is_empty(), "text is never compared");
    assert_eq!(target.replaced()[0].1.len(), 2);
}

#[tokio::test]
async fn a_rule_is_known_as_held_in_the_form_the_target_reads_it_back() {
    // A language that reads a forward back as a redirect keeping a copy, as Sieve does.
    fn read_back(rule: &MailRule) -> MailRule {
        let mut back = rule.clone();
        back.origin = None;
        if let RuleBody::Typed(typed) = &mut back.body {
            for action in &mut typed.actions {
                if let Action::Forward { to } = action {
                    *action = Action::Redirect {
                        to: to.clone(),
                        copy: true,
                    };
                }
            }
        }
        back
    }
    let forward = rule(
        "forward",
        vec![Action::Forward {
            to: vec![RuleAddress::new("away@example.com").unwrap()],
        }],
    );
    let mut target = Target::with_own(vec![read_back(&forward).disabled()]);
    target.vocabulary = vocabulary().with_exact([RuleFeature::RedirectCopy]).with(
        RuleFeature::Forward,
        engine_core::rules::Support::Approximated,
    );
    target.dialect = ImportDialect::new(|rule| rule, |rule| Some(read_back(rule)));
    let receipt = target
        .import_rules(
            &account(),
            std::slice::from_ref(&forward),
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.already_held, [0]);
    assert!(target.replaced().is_empty(), "nothing written");

    // Compared as handed in, it would be written again.
    target.dialect = ImportDialect::as_given();
    let receipt = target
        .import_rules(
            &account(),
            &[forward],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert!(receipt.already_held.is_empty(), "compared as given");
}

#[tokio::test]
async fn a_rule_carried_whole_is_planned_with_the_facts_its_text_gives() {
    // The host said nothing of what the text does; the language reads that it runs a script.
    let raw = engine_core::rules::RawRule::new(engine_core::rules::RuleSyntax::Sieve, "include x;")
        .requiring(Vec::new())
        .sending_no_mail();
    let mut target = Target::with_own(Vec::new());
    target.vocabulary = vocabulary().with_opaque(engine_core::rules::RuleSyntax::Sieve, Vec::new());
    target.dialect = ImportDialect::new(
        |mut rule| {
            if let RuleBody::Opaque(raw) = &rule.body {
                rule.body = RuleBody::Opaque(raw.clone().including_scripts());
            }
            rule
        },
        |rule| Some(rule.clone()),
    );
    let receipt = target
        .import_rules(
            &account(),
            &[MailRule::opaque(raw)],
            ForwardingRules::Disabled,
            &folders(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    assert!(target.replaced().is_empty(), "nothing written");
}

/// Reaches the accessor as a host generic over `P: Provider` does, through the `Box` impl.
fn importer_of<P: Provider + ?Sized>(provider: &P) -> Option<&dyn RulesImporter> {
    provider.rules_importer()
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_its_rules_importer() {
    let held: Box<dyn Provider> = Box::new(Target::with_own(Vec::new()));
    let importer = importer_of(&held).expect("an adapter that imports rules answers itself");
    let away = VacationResponse::default()
        .turned_on()
        .with_reply(AutoReply::default().with_text("Away"));
    let receipt = importer.import_vacation(&account(), &away).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v2")));

    let none: Box<dyn Provider> = Box::new(FakeJmap {
        info: ConnectionInfo::new(Capabilities::none().with_mail()),
    });
    assert!(importer_of(&none).is_none());
}
