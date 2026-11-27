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

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    mail::Keyword,
    raw::RawRules,
    rules::{
        MailRule, NativeKind, RuleFeature, RuleLimit, RuleSet, RuleVocabulary, RulesReceipt,
        Support, TextOp,
    },
};
use engine_provider::{MailRules, ProviderResult, RuleListing, WriteGuard};
use serde_json::Value;

use crate::{
    error::GoogleError, json::opt_str, named_labels::NamedLabels, provider::GmailProvider,
    rules_read, transport::GoogleClient,
};

/// The filters' collection.
pub(crate) const FILTERS: &str = "/gmail/v1/users/me/settings/filters";

/// What a Gmail filter holds (`filters-and-out-of-office.md`, §5, the Gmail column), with
/// `keywords` the keywords the host registered a label name for (`crate::named_labels`).
///
/// Exact: what Gmail's criteria say read as the model says it, a subject and the sender with its
/// name matched by words ([`TextOp::Words`]), any-of conditions and the exceptions (search `OR`),
/// an attachment, a size in bytes; filing into the user's labels, Trash, Junk and the archive;
/// read and flagged.
///
/// Approximated: a subject that contains a value, written as a word search, which matches less
/// (so held as a condition only: [`RuleLimit::ExactExceptions`]); a redirect that keeps the
/// message and a forward (Gmail's `forward`, which keeps the sender); a discard (to Trash, which
/// is recoverable); an importance (Gmail's own `IMPORTANT` marker); a named keyword (a label);
/// the order. Gmail applies every filter that matches, and has no stop, so the label changes and
/// the forwards are the same in any order: what the order did is kept, apart from a stop, which is
/// lost on its own.
///
/// Not held: an address alone (a word search over the sender matches its name too), the
/// recipients (Gmail's `to:` matches `Bcc` too), a subject that is a value, a named header, a body
/// (a search word matches the subject and the senders too), a domain or local part, the account's
/// own address, an importance tested, a folder by path, a redirect that keeps no copy, a stop, a
/// rule stored off, a name. Its limits ([`RuleLimit`]), each a refusal Gmail answers or a
/// widening the writer must not make: one forward per filter, a native forward included; one of
/// the user's labels per filter; a filter tests something; no system keyword but `$seen` and
/// `$flagged`; filing only into real folders, and into roles only Trash, Junk and the archive by
/// a move; an importance high or low; a native condition only among all-of conditions; every value
/// a phrase; a word match on an address only on the sender with its name.
#[must_use]
pub(crate) fn vocabulary(keywords: &BTreeSet<Keyword>) -> RuleVocabulary {
    let approximated = [
        RuleFeature::SubjectText(TextOp::Contains),
        RuleFeature::Discard,
        RuleFeature::RedirectCopy,
        RuleFeature::Forward,
        RuleFeature::MarkImportance,
        RuleFeature::Order,
    ];
    let limits = [
        RuleLimit::NoOtherSystemKeyword,
        RuleLimit::RealFoldersOnly,
        RuleLimit::OneFolderPerRule,
        RuleLimit::SomeTest,
        RuleLimit::ExactExceptions,
        RuleLimit::ImportanceHighOrLow,
        RuleLimit::NativeConditionsAllOf,
        RuleLimit::PhraseValues,
        RuleLimit::TrashJunkArchiveOnly,
        RuleLimit::WordsOnSubjectAndSender,
    ];
    let vocabulary = approximated
        .into_iter()
        .fold(RuleVocabulary::new(), |vocabulary, feature| {
            vocabulary.with(feature, Support::Approximated)
        })
        .with_exact([
            RuleFeature::MatchAny,
            RuleFeature::Except,
            RuleFeature::SubjectText(TextOp::Words),
            RuleFeature::AddressText(TextOp::Words),
            RuleFeature::AddressWithName,
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
        .with_keywords(keywords.iter().cloned());
    let vocabulary = limits
        .into_iter()
        .fold(vocabulary, RuleVocabulary::with_limit);
    if keywords.is_empty() {
        vocabulary
    } else {
        vocabulary.with(RuleFeature::NamedKeyword, Support::Approximated)
    }
}

/// The account's filters as Gmail listed them, sorted by id, and the text they are kept as.
pub(crate) struct Listing {
    pub(crate) filters: Vec<Value>,
    pub(crate) listing: RuleListing,
}

impl Listing {
    /// Reads the filters. Gmail answers them in one response, with no `filter` key at all where
    /// there are none, and `204` with no body (`null`) where the account has none (observed).
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
        let listing = RuleListing::new(Value::Array(filters.clone()).to_string());
        Self { filters, listing }
    }

    /// The listing kept in `raw`, as a read stored it.
    pub(crate) fn from_raw(raw: &RawRules) -> Option<Self> {
        match serde_json::from_str(raw.as_str()).ok()? {
            Value::Array(filters) => Some(Self::new(filters)),
            _ => None,
        }
    }

    /// The filter at `index`, as the model reads it.
    pub(crate) fn rule(&self, index: usize, named: &NamedLabels) -> Option<MailRule> {
        self.filters
            .get(index)
            .map(|value| rules_read::rule(value, self.listing.fingerprint(), index, named))
    }

    pub(crate) fn rule_set(&self, named: &NamedLabels) -> RuleSet {
        let rules = (0..self.filters.len())
            .filter_map(|index| self.rule(index, named))
            .collect();
        RuleSet::new(rules)
            .with_version(self.listing.version())
            .with_raw(self.listing.raw())
    }
}

#[async_trait]
impl MailRules for GmailProvider {
    async fn rule_vocabulary(&self, _account: &AccountId) -> ProviderResult<RuleVocabulary> {
        Ok(vocabulary(&self.labels().keywords()))
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

#[cfg(test)]
#[path = "rules_folder_tests.rs"]
mod folder_tests;
