//! Importing mail rules and the out-of-office reply over JMAP Sieve: [`RulesImporter`], through
//! the engine's one import ([`append_rules`], [`import_reply`]) over this adapter's own rules and
//! reply, written so they never stop what the account runs now ([`ImportTarget`]).
//!
//! What JMAP adds is the target's folders: the host maps each source folder id to the target's,
//! and the target ids the rules need are asked of the server (`Mailbox/get` for exactly those, in
//! chunks of `maxObjectsInGet`), so a mapped folder the account no longer has is lost as
//! `MissingMailbox` rather than written as `fileinto :mailboxid`, which would file into the inbox
//! (RFC 9042's fallback) instead.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    rules::{
        Action, FolderMap, ForwardingRules, MailRule, RuleBody, RuleMailbox, RuleSet,
        RuleVocabulary, RulesReceipt,
    },
    vacation::{VacationReceipt, VacationResponse, VacationVocabulary},
};
use engine_provider::{
    MailRules, Provider, ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard,
    import::{ImportDialect, RulesImporter, append_rules, import_reply},
};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
    rules::WhenItStops,
};

#[async_trait]
impl RulesImporter for JmapProvider {
    async fn import_rules(
        &self,
        account: &AccountId,
        rules: &[MailRule],
        forwarding: ForwardingRules,
        folders: &FolderMap,
    ) -> ProviderResult<RulesReceipt> {
        let wanted = named_targets(rules, folders);
        let held = held_mailboxes(self.executor(), &wanted).await?;
        let mut folders = folders.clone();
        folders.retain(|_, target| held.contains(target));
        append_rules(
            &ImportTarget(self),
            account,
            rules,
            forwarding,
            &folders,
            sieve_dialect(),
        )
        .await
    }

    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        if self.vacation().is_none() {
            return Err(ProviderError::item_unavailable(
                "this account keeps no out-of-office reply (no vacationresponse capability)",
            ));
        }
        import_reply(&ImportTarget(self), account, reply).await
    }
}

/// The Sieve dialect as the import reads it: facts of a rule carried whole from its text, and a
/// rule as the account will read it back (`engine_sieve::dialect`).
///
/// The ManageSieve importer (`provider-imap`) builds the same pair. It is built twice because
/// neither place one constructor could live is reachable by both without a new crate edge:
/// `ImportDialect` is `engine-provider`'s and the two functions are `engine-sieve`'s, which do not
/// depend on each other, and neither adapter depends on the other.
fn sieve_dialect() -> ImportDialect {
    ImportDialect::new(
        engine_sieve::dialect::facts_from_text,
        engine_sieve::dialect::read_back,
    )
}

/// This adapter's rules and reply as an import writes them: read as ever, written by the same
/// paths as an edit, but refused before anything is sent where the write would stop what runs now
/// ([`WhenItStops::Refuse`]): rules that would stop the running reply, a reply that would switch
/// the running rules off. An edit writes those and says so on its receipt; an import never changes
/// what the target had.
struct ImportTarget<'a>(&'a JmapProvider);

#[async_trait]
impl MailRules for ImportTarget<'_> {
    async fn rule_vocabulary(&self, account: &AccountId) -> ProviderResult<RuleVocabulary> {
        self.0.rule_vocabulary(account).await
    }

    fn rules_write_guard(&self) -> WriteGuard {
        self.0.rules_write_guard()
    }

    async fn read_rules(&self, account: &AccountId) -> ProviderResult<RuleSet> {
        self.0.read_rules(account).await
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        crate::rules::replace(self.0, base, rules, WhenItStops::Refuse).await
    }
}

#[async_trait]
impl Vacation for ImportTarget<'_> {
    async fn vocabulary(&self, account: &AccountId) -> ProviderResult<VacationVocabulary> {
        Vacation::vocabulary(self.0, account).await
    }

    fn vacation_write_guard(&self) -> WriteGuard {
        self.0.vacation_write_guard()
    }

    async fn read_vacation(&self, account: &AccountId) -> ProviderResult<VacationResponse> {
        self.0.read_vacation(account).await
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        crate::vacation::set(self.0, precondition, new, WhenItStops::Refuse).await
    }
}

/// The target folders `rules` file into, through `folders`.
fn named_targets(rules: &[MailRule], folders: &FolderMap) -> BTreeSet<MailboxId> {
    rules
        .iter()
        .filter_map(|rule| match &rule.body {
            RuleBody::Typed(typed) => Some(&typed.actions),
            _ => None,
        })
        .flatten()
        .filter_map(|action| match action {
            Action::FileInto {
                mailbox: RuleMailbox::Id(source),
                ..
            } => folders.target(source).cloned(),
            _ => None,
        })
        .collect()
}

/// Which of `named` the account has: `Mailbox/get` for exactly those ids, as many to a call as
/// the session's `maxObjectsInGet` allows, none where no rule names one. An id the server answers
/// `notFound` is left out.
async fn held_mailboxes(
    executor: &dyn Executor,
    named: &BTreeSet<MailboxId>,
) -> Result<BTreeSet<MailboxId>, JmapError> {
    let session = executor.session();
    let most = session.limits().max_objects_in_get.max(1);
    let ids: Vec<&str> = named.iter().map(MailboxId::as_str).collect();
    let mut held = BTreeSet::new();
    for chunk in ids.chunks(most) {
        let account = session.mail_account_id()?;
        let mut request = Request::new([capability::CORE, capability::MAIL]);
        let get = request.invoke(
            "Mailbox/get",
            json!({ "accountId": account, "ids": chunk, "properties": ["id"] }),
        );
        let response = executor.execute(&request).await?;
        let list = response
            .result(&get)?
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| JmapError::protocol("a Mailbox/get answer without a list"))?;
        held.extend(
            list.iter()
                .filter_map(|mailbox| mailbox.get("id")?.as_str())
                .filter_map(|id| MailboxId::try_from(id).ok())
                .filter(|id| named.contains(id)),
        );
    }
    Ok(held)
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "rules_held_tests.rs"]
mod held_tests;
