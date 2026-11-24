//! Importing mail rules and the out-of-office reply over ManageSieve: [`RulesImporter`], through
//! the engine's one import ([`append_rules`], [`import_reply`]) over this service's own
//! [`MailRules`](engine_provider::MailRules) and [`Vacation`](engine_provider::Vacation).
//!
//! No folder is known by id here. A Sieve `fileinto :mailboxid` names a folder by its RFC 8474
//! `MAILBOXID`, which this adapter does not read: its folder ids are their names. So a rule that
//! files by id is lost as `MissingMailbox` rather than written with an id the server would not
//! know (it would file into the inbox, RFC 9042's fallback); a host names an IMAP target's
//! folders by path.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    rules::{ForwardingRules, MailRule, RulesReceipt},
    vacation::{VacationReceipt, VacationResponse},
};
use engine_provider::{
    ProviderResult,
    import::{RulesImporter, append_rules, import_reply},
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::service::ManageSieve;

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> RulesImporter for ManageSieve<T> {
    async fn import_rules(
        &self,
        account: &AccountId,
        rules: &[MailRule],
        forwarding: ForwardingRules,
    ) -> ProviderResult<RulesReceipt> {
        append_rules(self, account, rules, forwarding, &BTreeSet::new()).await
    }

    /// Refused, as [`Vacation`](engine_provider::Vacation) refuses, where the server lists no
    /// `vacation`.
    async fn import_vacation(
        &self,
        account: &AccountId,
        reply: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        import_reply(self, account, reply).await
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
