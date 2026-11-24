//! Importing mail rules and the out-of-office reply over ManageSieve: [`RulesImporter`], through
//! the engine's one import ([`append_rules`], [`import_reply`]) over this service's own
//! [`MailRules`](engine_provider::MailRules) and [`Vacation`](engine_provider::Vacation).
//!
//! No rule files by id here (`crate::managesieve::rules`): a Sieve `fileinto :mailboxid` names a
//! folder by its RFC 8474 `MAILBOXID`, which this adapter does not read, its folder ids being
//! their names. So a rule filing by id loses that action, as `MissingMailbox` where the host's
//! folder map does not hold the folder and as `NotExpressible` where it does, rather than being
//! written with an id the server would not know (it would file into the inbox, RFC 9042's
//! fallback); a host names an IMAP target's folders by path.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    rules::{FolderMap, ForwardingRules, MailRule, RulesReceipt},
    vacation::{VacationReceipt, VacationResponse},
};
use engine_provider::{
    ProviderResult,
    import::{ImportDialect, RulesImporter, append_rules, import_reply},
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
        folders: &FolderMap,
    ) -> ProviderResult<RulesReceipt> {
        // The Sieve dialect: facts of a rule carried whole from its text, and a rule as the account
        // will read it back. The JMAP importer (`provider-jmap`) builds the same pair; it is built
        // twice because `ImportDialect` (`engine-provider`) and the two functions (`engine-sieve`)
        // do not depend on each other, and neither adapter depends on the other.
        let dialect = ImportDialect::new(
            engine_sieve::dialect::facts_from_text,
            engine_sieve::dialect::read_back,
        );
        append_rules(self, account, rules, forwarding, folders, dialect).await
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
