//! The account's Gmail filters, as the neutral [`MailRules`] (`filters-and-out-of-office.md`;
//! `google.md`, "Filters and the vacation reply").
//!
//! - **Read** with `users.settings.filters.list` into the model by `crate::rules_read`. Gmail lists
//!   filters in no stated order and applies every one that matches, so the listing is kept sorted
//!   by id, and the rules have no order.
//! - **The vocabulary is fixed** ([`vocabulary`]): the search subset Gmail's criteria are read in,
//!   label changes and one forward. Every filter runs, has no name and no stop.
//! - **No write guard on the server.** A filter carries no ETag and cannot be changed, only created
//!   and deleted, so the guard is [`WriteGuard::Absent`]. The version is a SHA-256 of the listing
//!   as read, and a write reads it again and refuses as `Conflict` where it moved: that narrows the
//!   race with another client to the time the write takes, and does not close it.
//! - **Written filter by filter** (`crate::rules_apply`): each failure lands in the receipt, and
//!   the rest of the write goes ahead.
//!
//! Reading needs `https://mail.google.com/`, `gmail.modify`, `gmail.readonly` or
//! `gmail.settings.basic`; writing needs `gmail.settings.basic`, and a token without it is
//! refused (`403`, `Permanent`) on the first write.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawRules,
    rules::{
        MailRule, NativeKind, RuleFeature, RuleLimit, RuleSet, RuleVocabulary, RulesReceipt,
        Support, TextOp,
    },
    version::RuleSetVersion,
};
use engine_provider::{MailRules, ProviderResult, WriteGuard};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    error::GoogleError, json::opt_str, named_labels::NamedLabels, provider::GmailProvider,
    rules_read, transport::GoogleClient,
};

/// The filters' collection.
pub(crate) const FILTERS: &str = "/gmail/v1/users/me/settings/filters";

/// What a Gmail filter holds (`filters-and-out-of-office.md`, §5, the Gmail column), with
/// `keywords` where the host registered a label name for a keyword (`crate::named_labels`).
///
/// Approximated, because Gmail matches criteria as a search, by words and phrases, `from:` over
/// the sender's name too and `to:` over every recipient: a subject, sender or the recipients
/// together that contain or are a value, any-of conditions and exceptions (search `OR`); a
/// redirect that keeps the message and a forward (Gmail's `forward`, which keeps the sender); a
/// discard (to Trash, which is recoverable); an importance (Gmail's own `IMPORTANT` marker); a
/// named keyword (a label).
///
/// Not held: a subject that is a value (Gmail's subject search would match it within a longer
/// one, which widens the rule), a named header, a body (a search word matches the subject and the
/// senders too, which would widen the rule), a domain or local part alone, the account's own
/// address, an importance tested, a folder by path, a redirect that keeps no copy, a stop, a rule
/// stored off, a name and an order. One forward address per rule
/// ([`RuleVocabulary::with_max_redirects`]), the recipients only together, no system keyword but
/// `$seen` and `$flagged`, and filing only into real folders ([`RuleLimit`]).
#[must_use]
pub(crate) fn vocabulary(keywords: bool) -> RuleVocabulary {
    let approximated = [
        RuleFeature::MatchAny,
        RuleFeature::Except,
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::AddressText(TextOp::Contains),
        RuleFeature::AddressText(TextOp::Is),
        RuleFeature::Discard,
        RuleFeature::RedirectCopy,
        RuleFeature::Forward,
        RuleFeature::MarkImportance,
    ];
    let vocabulary = approximated
        .into_iter()
        .fold(RuleVocabulary::new(), |vocabulary, feature| {
            vocabulary.with(feature, Support::Approximated)
        })
        .with_exact([
            RuleFeature::HasAttachment,
            RuleFeature::SizeOver,
            RuleFeature::SizeUnder,
            RuleFeature::FileIntoId,
            RuleFeature::FileIntoCopy,
            RuleFeature::FileIntoRole,
            RuleFeature::MarkRead,
            RuleFeature::Flag,
        ])
        .with_max_redirects(1)
        .with_native(NativeKind::Gmail)
        .with_limit(RuleLimit::RecipientsTogether)
        .with_limit(RuleLimit::NoOtherSystemKeyword)
        .with_limit(RuleLimit::RealFoldersOnly)
        .with_limit(RuleLimit::OneFolderPerRule)
        .with_limit(RuleLimit::SomeTest);
    if keywords {
        vocabulary.with(RuleFeature::NamedKeyword, Support::Approximated)
    } else {
        vocabulary
    }
}

/// The account's filters as Gmail listed them, sorted by id, and the text they are kept as.
pub(crate) struct Listing {
    pub(crate) filters: Vec<Value>,
    text: String,
}

impl Listing {
    /// Reads the filters. Gmail answers them in one response, with no `filter` key at all where
    /// there are none.
    pub(crate) async fn read(client: &GoogleClient) -> Result<Self, GoogleError> {
        let page = client.get(&client.url(FILTERS)).await?;
        let filters = match page.get("filter") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(filters)) => filters.clone(),
            Some(_) => return Err(GoogleError::protocol("filters.list filter is not a list")),
        };
        Ok(Self::new(filters))
    }

    fn new(mut filters: Vec<Value>) -> Self {
        // The order Gmail lists them in must not move the version, or a write would see a
        // conflict nothing caused.
        filters.sort_by(|a, b| opt_str(a, "id").cmp(&opt_str(b, "id")));
        let text = Value::Array(filters.clone()).to_string();
        Self { filters, text }
    }

    /// The listing kept in `raw`, as a read stored it.
    pub(crate) fn from_raw(raw: &RawRules) -> Option<Self> {
        match serde_json::from_str(raw.as_str()).ok()? {
            Value::Array(filters) => Some(Self::new(filters)),
            _ => None,
        }
    }

    /// The version a write names: the SHA-256 of the listing, in hex.
    pub(crate) fn version(&self) -> String {
        hex(&Sha256::digest(self.text.as_bytes()))
    }

    /// The fingerprint a rule's origin carries.
    pub(crate) fn set(&self) -> u64 {
        let digest = Sha256::digest(self.text.as_bytes());
        let mut first = [0; 8];
        first.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(first)
    }

    /// The filter at `index`, as the model reads it.
    pub(crate) fn rule(&self, index: usize, named: &NamedLabels) -> Option<MailRule> {
        self.filters
            .get(index)
            .map(|value| rules_read::rule(value, self.set(), index, named))
    }

    pub(crate) fn rule_set(&self, named: &NamedLabels) -> RuleSet {
        let rules = (0..self.filters.len())
            .filter_map(|index| self.rule(index, named))
            .collect();
        RuleSet::new(rules)
            .with_version(RuleSetVersion::new(self.version()))
            .with_raw(RawRules::new(self.text.clone()))
    }
}

/// Bytes as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[async_trait]
impl MailRules for GmailProvider {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(vocabulary(self.labels().any()))
    }

    /// [`WriteGuard::Absent`]: a filter has no ETag. A write still refuses a base whose listing
    /// moved, as read again just before it writes (the module docs).
    fn rules_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_rules(&self, _account: &AccountId) -> ProviderResult<RuleSet> {
        let named = self.labels().resolved(self.client()).await?;
        Ok(Listing::read(self.client()).await?.rule_set(&named))
    }

    async fn replace_rules(
        &self,
        _account: &AccountId,
        base: &RuleSet,
        rules: &[MailRule],
    ) -> ProviderResult<RulesReceipt> {
        crate::rules_apply::replace(self.client(), self.labels(), base, rules).await
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
