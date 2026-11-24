//! Replacing the account's filters, one at a time (`filters-and-out-of-office.md`, §4: "Graph and
//! Gmail adapters diff `base` against `rules`").
//!
//! 1. **Planned** by `plan_rules` in edit mode against the Gmail vocabulary
//!    (`crate::rules::vocabulary`), whose features and limits say what a filter cannot hold.
//! 2. **Refused before sending** where `base` was not read (no version, or a raw form that is not
//!    the one its version names: `Permanent`), or where the listing moved since (`Conflict`).
//! 3. **Written, new first.** Gmail has no update: a changed rule is created as a new filter, and
//!    the filter it was read as is deleted only once the new one is stored. A rule whose filter
//!    Gmail already holds, the one it was read as or another no rule claims, or one this write
//!    created, is not sent: Gmail refuses a filter identical to one it holds (observed), so the one
//!    held is kept for it. A forward to an address the account has not verified is lost
//!    (`ForwardNotAllowed`) before anything is sent: Gmail forwards only to an address it has
//!    verified (observed).
//! 4. **Never removes what it does not replace.** A rule the plan does not keep, or the server
//!    refuses, is not written, and the filter it was read as stays as it was: the receipt names it
//!    ([`RulesReceipt::left_as_read`]). Only then are the filters no rule names deleted (one
//!    already gone is the state asked for).
//! 5. **No orphan labels.** A label made for a keyword of a filter that was then not stored is
//!    deleted again, as far as Gmail lets it.
//!
//! Each request's failure is a [`RuleWriteFailure`] in the receipt, by position and class, and the
//! rest go ahead; the rule's outcome says so too ([`LossReason::NotWritten`]). The receipt carries
//! no version: the next write reads first.
//!
//! [`LossReason::NotWritten`]: engine_core::rules::LossReason::NotWritten

use std::collections::{BTreeMap, BTreeSet};

use engine_core::rules::{
    FailedRule, Kept, MailRule, PlanMode, RuleOutcome, RulePart, RuleSet, RuleWriteFailure,
    RulesReceipt, plan_rules,
};
use engine_provider::{ProviderError, ProviderResult, RuleListing};
use serde_json::Value;

use crate::{
    error::GoogleError,
    json::opt_str,
    named_labels::Labels,
    rules::{FILTERS, Listing, vocabulary},
    rules_apply_support::{Held, failure, refused, same_filter, with_outcome},
    transport::{GoogleClient, encode_query_value},
};

pub(crate) async fn replace(
    client: &GoogleClient,
    labels: &Labels,
    base: &RuleSet,
    rules: &[MailRule],
) -> ProviderResult<RulesReceipt> {
    let plan = plan_rules(rules, &vocabulary(&labels.keywords()), PlanMode::Edit);
    let read = RuleListing::base_raw(base)?;
    Listing::from_raw(read).ok_or_else(RuleListing::unread)?;
    let now = Listing::read(client).await?;
    if Some(&now.listing.version()) != base.version.as_ref() {
        return Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ));
    }
    let named = labels.resolved(client).await?;
    let len = now.filters.len();
    let mut claimed = BTreeMap::new();
    let mut unchanged = BTreeSet::new();
    let mut order = Vec::with_capacity(plan.rules.len());
    for planned in &plan.rules {
        let existing = now
            .listing
            .index_of(&planned.rule, len)
            .filter(|index| !claimed.contains_key(index));
        if let Some(index) = existing {
            claimed.insert(index, planned.source);
            if now.rule(index, &named).as_ref() == Some(&planned.rule) {
                unchanged.insert(index);
            }
        }
        order.push((planned.source, &planned.rule, existing));
    }
    // A rule the plan does not keep is not written: the filter it was read as stays as it was.
    for outcome in plan.outcomes.iter().filter(|o| o.kept == Kept::NotKept) {
        if let Some(index) = now.listing.index_of(&rules[outcome.source], len) {
            claimed.entry(index).or_insert(outcome.source);
        }
    }
    let mut write = Write {
        client,
        labels,
        now: &now,
        outcomes: plan.outcomes,
        failures: Vec::new(),
        forwarding: None,
        claimed,
        unchanged,
        created: Vec::new(),
        made: BTreeMap::new(),
    };
    for (source, rule, existing) in order {
        if !existing.is_some_and(|index| write.unchanged.contains(&index)) {
            write.one(source, &rules[source], rule, existing).await;
        }
    }
    // Only now the filters no rule names: one claimed by a rule was replaced or kept above.
    let unclaimed: Vec<usize> = (0..len)
        .filter(|index| !write.claimed.contains_key(index))
        .collect();
    for index in unclaimed {
        write.delete(index).await;
    }
    write.tidy_labels().await;
    let claims = &write.claimed;
    let left = now
        .listing
        .left_as_read(rules, &write.outcomes, len, |source, index| {
            claims.get(&index) == Some(&source)
        });
    Ok(RulesReceipt::new(None, write.outcomes)
        .with_failures(write.failures)
        .with_left_as_read(left))
}

pub(crate) struct Write<'a> {
    pub(crate) client: &'a GoogleClient,
    pub(crate) labels: &'a Labels,
    pub(crate) now: &'a Listing,
    pub(crate) outcomes: Vec<RuleOutcome>,
    pub(crate) failures: Vec<RuleWriteFailure>,
    /// The addresses the account forwards to, once asked.
    pub(crate) forwarding: Option<BTreeSet<String>>,
    /// Which rule handed in each filter read stays for, or is replaced by.
    pub(crate) claimed: BTreeMap<usize, usize>,
    /// The filters read that a rule keeps as they are.
    pub(crate) unchanged: BTreeSet<usize>,
    /// The filters this write created.
    pub(crate) created: Vec<Value>,
    /// The labels this write made for a keyword, and whether a stored filter adds one.
    pub(crate) made: BTreeMap<String, bool>,
}

impl Write<'_> {
    /// Writes one planned rule, as given at `source` and as planned, in place of the filter at
    /// `existing`, if any.
    async fn one(
        &mut self,
        source: usize,
        given: &MailRule,
        planned: &MailRule,
        existing: Option<usize>,
    ) {
        let rule = match self.forwardable(source, given, planned).await {
            Ok(Some(rule)) => rule,
            Ok(None) => return,
            Err(failed) => return self.unwritten(source, failed),
        };
        let Some(filter) = self.filter(source, &rule).await else {
            return;
        };
        match self.held(&filter, existing) {
            Held::AsRead => return,
            Held::Unclaimed(index) => {
                self.claimed.insert(index, source);
            }
            Held::Kept => {}
            Held::Nowhere => {
                let url = self.client.url(FILTERS);
                let body = filter.to_string().into_bytes();
                let posted = self.client.post(&url, "application/json", body).await;
                if let Err(err) = GoogleError::readable(posted) {
                    return self.unwritten(source, failure(source, &err));
                }
                self.stored(&filter);
            }
        }
        // The filter asked for is there: the one it was read as goes.
        if let Some(index) = existing
            && !self.delete(index).await
        {
            // The old filter stays beside the new one: the outcome says so too.
            self.outcomes[source].left_beside();
        }
    }

    /// Where Gmail already holds `filter`: the filter the rule was read as, one read that no rule
    /// claims or that a rule keeps as it is, or one this write created.
    fn held(&self, filter: &Value, existing: Option<usize>) -> Held {
        if existing.is_some_and(|index| same_filter(&self.now.filters[index], filter)) {
            return Held::AsRead;
        }
        for (index, held) in self.now.filters.iter().enumerate() {
            if !same_filter(held, filter) {
                continue;
            }
            if !self.claimed.contains_key(&index) {
                return Held::Unclaimed(index);
            }
            if self.unchanged.contains(&index) {
                return Held::Kept;
            }
        }
        if self
            .created
            .iter()
            .any(|created| same_filter(created, filter))
        {
            return Held::Kept;
        }
        Held::Nowhere
    }

    /// Records a filter stored, and the labels it adds as used.
    fn stored(&mut self, filter: &Value) {
        for id in filter["action"]["addLabelIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(used) = self.made.get_mut(id) {
                *used = true;
            }
        }
        self.created.push(filter.clone());
    }

    /// Records that the rule at `source` was not written. The filter it claimed stays as it was.
    pub(crate) fn unwritten(&mut self, source: usize, failed: RuleWriteFailure) {
        self.failures.push(failed);
        self.outcomes[source].not_written(RulePart::Rule);
    }

    /// `rule` as a filter, or `None` where it is not written, the failure recorded: a keyword's
    /// label could not be found or made, or the writer cannot place a part.
    async fn filter(&mut self, source: usize, rule: &MailRule) -> Option<Value> {
        let mut label = BTreeMap::new();
        for keyword in crate::rules_apply_support::named_keywords(rule) {
            match self.labels.label_id(self.client, &keyword).await {
                Some(Ok((id, made))) => {
                    if made {
                        self.made.insert(id.clone(), false);
                    }
                    label.insert(keyword, id);
                }
                Some(Err(err)) => {
                    self.unwritten(source, failure(source, &err));
                    return None;
                }
                None => {}
            }
        }
        let filter = crate::rules_write::filter(rule, &label).ok();
        if filter.is_none() {
            self.unwritten(source, refused(source));
        }
        filter
    }

    /// Deletes the labels this write made for a filter it then did not store. Best effort: a label
    /// Gmail will not delete stays, holding no message and named in no filter.
    async fn tidy_labels(&mut self) {
        let unused: Vec<String> = self
            .made
            .iter()
            .filter(|(_, used)| !**used)
            .map(|(id, _)| id.clone())
            .collect();
        for id in unused {
            let _ = self.labels.unmake(self.client, &id).await;
        }
    }

    /// `DELETE`s the filter at `index`; whether it is gone, one already gone included.
    async fn delete(&mut self, index: usize) -> bool {
        let Some(id) = opt_str(&self.now.filters[index], "id") else {
            self.failures.push(RuleWriteFailure::new(
                FailedRule::Removed(index),
                engine_core::error::FailureClass::Permanent,
            ));
            return false;
        };
        let url = self
            .client
            .url(&format!("{FILTERS}/{}", encode_query_value(id)));
        match self.client.delete(&url, None).await {
            Ok(()) | Err(GoogleError::Status { status: 404, .. }) => true,
            Err(err) => {
                let failed = with_outcome(
                    RuleWriteFailure::new(FailedRule::Removed(index), err.failure_class()),
                    &err,
                );
                self.failures.push(failed);
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "rules_apply_tests.rs"]
mod tests;
