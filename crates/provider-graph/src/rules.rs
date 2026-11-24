//! The account's mail rules over Microsoft Graph, as the neutral [`MailRules`]
//! (`filters-and-out-of-office.md`; `graph.md`, "Mail rules and the out-of-office reply").
//!
//! - **Inbox rules only.** Graph keeps an account's rules as `messageRule` objects under
//!   `{root}/mailFolders/inbox/messageRules`, `{root}` the client's principal, so `/me` and
//!   `/users/{address}` read the same way. Read into the model by `crate::rules_read`, in
//!   `sequence` order.
//! - **The vocabulary is a constant** ([`vocabulary`]): Graph's predicate and action set is fixed.
//! - **No write guard on the server.** A `messageRule` carries no `@odata.etag` (observed: neither
//!   the listing nor a single rule has one), so there is nothing to send as `If-Match`, and the
//!   guard is [`WriteGuard::Absent`]. The version is a SHA-256 of the listing as read, and a write
//!   reads the listing again and refuses as `Conflict` when it moved: that narrows the race with
//!   another client to the time the write takes, and does not close it.
//! - **Written rule by rule** (`crate::rules_apply`): each failure lands in the receipt, and the
//!   rest of the write goes ahead.
//!
//! A `403` is the credential's grant falling short, `Permanent` as every Graph `403`: a delegated
//! token without `MailboxSettings.Read` (a personal account's, observed), or another user's
//! mailbox on a credential that may not read its settings. Signing in again with the scope is
//! the way out of the first.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawRules,
    rules::{
        MailRule, NativeKind, RuleFeature, RuleSet, RuleVocabulary, RulesReceipt, Support, TextOp,
    },
    version::RuleSetVersion,
};
use engine_provider::{MailRules, ProviderResult, WriteGuard};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::GraphError, json::opt_str, provider::GraphProvider, rules_read, transport::GraphClient,
};

/// The rules' collection, under the client's principal.
pub(crate) const RULES: &str = "/mailFolders/inbox/messageRules";

/// What Graph's rule language holds (`filters-and-out-of-office.md`, §5, the Graph column).
///
/// Approximated: a domain is tested as `@domain` within the sender's or recipients' text, and a
/// named keyword is assigned as a category of its name. Not held: any-of conditions, a named
/// header (Graph's `headerContains` searches every header, which would widen the rule), a local
/// part, a test other than "contains" or, for addresses, equality, a size (`withinSizeRange` is
/// read native: its kilobyte bounds are not proven to mean what bytes do), a folder by path, a
/// redirect that keeps no copy (Exchange's always keeps one), and a flag.
#[must_use]
pub(crate) fn vocabulary() -> RuleVocabulary {
    RuleVocabulary::new()
        .with_exact([
            RuleFeature::Except,
            RuleFeature::SubjectText(TextOp::Contains),
            RuleFeature::Body(TextOp::Contains),
            RuleFeature::AddressText(TextOp::Is),
            RuleFeature::AddressText(TextOp::Contains),
            RuleFeature::SentToMe,
            RuleFeature::HasAttachment,
            RuleFeature::Importance,
            RuleFeature::FileIntoId,
            RuleFeature::FileIntoCopy,
            RuleFeature::FileIntoRole,
            RuleFeature::Discard,
            RuleFeature::RedirectCopy,
            RuleFeature::Forward,
            RuleFeature::MarkRead,
            RuleFeature::MarkImportance,
            RuleFeature::Stop,
            RuleFeature::Disabled,
            RuleFeature::DisabledMultiLine,
            RuleFeature::Name,
            RuleFeature::Order,
        ])
        .with(RuleFeature::AddressDomain, Support::Approximated)
        .with(RuleFeature::NamedKeyword, Support::Approximated)
        .with_native(NativeKind::Graph)
}

/// The account's rules as Graph listed them, in `sequence` order, and the text they are kept as.
pub(crate) struct Listing {
    pub(crate) rules: Vec<Value>,
    text: String,
}

impl Listing {
    /// Reads every page of the rules.
    pub(crate) async fn read(client: &GraphClient) -> Result<Self, GraphError> {
        let mut rules = Vec::new();
        let mut url = client.url(RULES);
        loop {
            let page = client.get(&url).await?;
            let values = page
                .get("value")
                .and_then(Value::as_array)
                .ok_or_else(|| GraphError::protocol("messageRules response had no value array"))?;
            rules.extend(values.iter().cloned());
            match opt_str(&page, "@odata.nextLink") {
                Some(next) => next.clone_into(&mut url),
                None => break,
            }
        }
        Ok(Self::new(rules))
    }

    fn new(mut rules: Vec<Value>) -> Self {
        // A rule without a sequence sorts last; the sort is stable, so ties keep Graph's order.
        rules.sort_by_key(|rule| {
            rule.get("sequence")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX)
        });
        let text = Value::Array(rules.clone()).to_string();
        Self { rules, text }
    }

    /// The listing kept in `raw`, as a read stored it.
    pub(crate) fn from_raw(raw: &RawRules) -> Option<Self> {
        match serde_json::from_str(raw.as_str()).ok()? {
            Value::Array(rules) => Some(Self::new(rules)),
            _ => None,
        }
    }

    fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.text.as_bytes()).into()
    }

    /// The version a write names: the SHA-256 of the listing, in hex.
    pub(crate) fn version(&self) -> String {
        crate::json::digest_hex(&self.text)
    }

    /// The fingerprint a rule's origin carries.
    pub(crate) fn set(&self) -> u64 {
        let digest = self.digest();
        let mut first = [0; 8];
        first.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(first)
    }

    /// The rule at `index`, as the model reads it.
    pub(crate) fn rule(&self, index: usize) -> Option<MailRule> {
        self.rules
            .get(index)
            .map(|value| rules_read::rule(value, self.set(), index))
    }

    pub(crate) fn rule_set(&self) -> RuleSet {
        let rules = (0..self.rules.len())
            .filter_map(|index| self.rule(index))
            .collect();
        RuleSet::new(rules)
            .with_version(RuleSetVersion::new(self.version()))
            .with_raw(RawRules::new(self.text.clone()))
    }
}

#[async_trait]
impl MailRules for GraphProvider {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(vocabulary())
    }

    /// [`WriteGuard::Absent`]: a `messageRule` has no ETag. A write still refuses a base whose
    /// listing moved, as read again just before it writes ([module docs](self)).
    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        Ok(Listing::read(self.client()).await?.rule_set())
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        crate::rules_apply::replace(self.client(), base, rules).await
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
