//! Importing mail rules and the out-of-office reply over JMAP Sieve: [`RulesImporter`], through
//! the engine's one import ([`append_rules`], [`import_reply`]) over this adapter's own
//! [`MailRules`](engine_provider::MailRules) and [`Vacation`](engine_provider::Vacation).
//!
//! What JMAP adds is the target's folders: a rule that files by id names a `Mailbox` id, so the
//! ids the rules name are asked of the server (`Mailbox/get` for exactly those), and one it does
//! not have is lost as `MissingMailbox` rather than written as `fileinto :mailboxid`, which would
//! file into the inbox (RFC 9042's fallback) instead.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    rules::{Action, ForwardingRules, MailRule, RuleBody, RuleMailbox, RulesReceipt},
    vacation::{VacationReceipt, VacationResponse},
};
use engine_provider::{
    Provider, ProviderError, ProviderResult,
    import::{RulesImporter, append_rules, import_reply},
};
use serde_json::{Value, json};

use crate::{
    JmapProvider,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

#[async_trait]
impl RulesImporter for JmapProvider {
    async fn import_rules(
        &self,
        account: &AccountId,
        rules: &[MailRule],
        forwarding: ForwardingRules,
    ) -> ProviderResult<RulesReceipt> {
        let mailboxes = held_mailboxes(self.executor(), &named_mailboxes(rules)).await?;
        append_rules(self, account, rules, forwarding, &mailboxes).await
    }

    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        let Some(target) = self.vacation() else {
            return Err(ProviderError::item_unavailable(
                "this account keeps no out-of-office reply (no vacationresponse capability)",
            ));
        };
        import_reply(target, account, reply).await
    }
}

/// The folder ids `rules` file into.
fn named_mailboxes(rules: &[MailRule]) -> BTreeSet<MailboxId> {
    rules
        .iter()
        .filter_map(|rule| match &rule.body {
            RuleBody::Typed(typed) => Some(&typed.actions),
            _ => None,
        })
        .flatten()
        .filter_map(|action| match action {
            Action::FileInto {
                mailbox: RuleMailbox::Id(id),
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// Which of `named` the account has: one `Mailbox/get` for exactly those ids, none where no rule
/// names one. An id the server answers `notFound` is left out.
async fn held_mailboxes(
    executor: &dyn Executor,
    named: &BTreeSet<MailboxId>,
) -> Result<BTreeSet<MailboxId>, JmapError> {
    if named.is_empty() {
        return Ok(BTreeSet::new());
    }
    let account = executor.session().mail_account_id()?;
    let ids: Vec<&str> = named.iter().map(MailboxId::as_str).collect();
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    let get = request.invoke(
        "Mailbox/get",
        json!({ "accountId": account, "ids": ids, "properties": ["id"] }),
    );
    let response = executor.execute(&request).await?;
    let list = response
        .result(&get)?
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| JmapError::protocol("a Mailbox/get answer without a list"))?;
    Ok(list
        .iter()
        .filter_map(|mailbox| mailbox.get("id")?.as_str())
        .filter_map(|id| MailboxId::try_from(id).ok())
        .filter(|id| named.contains(id))
        .collect())
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
