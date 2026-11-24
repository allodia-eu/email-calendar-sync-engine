//! Replacing the account's filters, one at a time (`filters-and-out-of-office.md`, §4: "Graph and
//! Gmail adapters diff `base` against `rules`").
//!
//! 1. **Planned** by `plan_rules` in edit mode against the Gmail vocabulary
//!    (`crate::rules::vocabulary`), whose features and limits say what a filter cannot hold.
//! 2. **Refused before sending** where `base` was not read (no version, or a raw form that is not
//!    the one its version names: `Permanent`), or where the listing moved since (`Conflict`).
//! 3. **Written, new first.** Gmail has no update: a changed rule is created as a new filter, and
//!    the filter it was read as is deleted only once the new one is stored. A rule equal to the
//!    filter it was read as is not sent. A rule forwarding to an address the account has not
//!    verified loses that action (`ForwardNotAllowed`) before anything is sent: Gmail forwards only
//!    to an address it has verified.
//! 4. **Never removes what it does not replace.** A rule the plan does not keep, or the writer
//!    cannot place, or the server refuses, is not written, and the filter it was read as stays as
//!    it was; the receipt lists it as a failure. Only then are the filters no rule names deleted
//!    (one already gone is the state asked for).
//!
//! Each request's failure is a [`RuleWriteFailure`] in the receipt, by position and class, and the
//! rest go ahead; the rule's outcome says so too ([`LossReason::NotWritten`]). The receipt carries
//! no version: the next write reads first.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    error::FailureClass,
    mail::Keyword,
    rules::{
        Action, FailedRule, Kept, Loss, LossReason, MailRule, PlanMode, RuleBody, RuleOrigin,
        RuleOutcome, RulePart, RuleSet, RuleWriteFailure, RulesReceipt, plan_rules,
    },
};
use engine_provider::{ProviderError, ProviderResult};
use serde_json::Value;

use crate::{
    error::GoogleError,
    json::opt_str,
    named_labels::{Labels, NamedLabels},
    rules::{FILTERS, Listing, vocabulary},
    rules_write,
    transport::{GoogleClient, encode_query_value},
};

/// The account's forwarding addresses.
const FORWARDING: &str = "/gmail/v1/users/me/settings/forwardingAddresses";

pub(crate) async fn replace(
    client: &GoogleClient,
    labels: &Labels,
    base: &RuleSet,
    rules: &[MailRule],
) -> ProviderResult<RulesReceipt> {
    let plan = plan_rules(rules, &vocabulary(labels.any()), PlanMode::Edit);
    let read = refuse_unread(base)?;
    let now = Listing::read(client).await?;
    if now.version() != read.version() {
        return Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ));
    }
    let mut write = Write {
        client,
        labels,
        named: labels.resolved(client).await?,
        now: &now,
        outcomes: plan.outcomes,
        failures: Vec::new(),
        forwarding: None,
        keep: BTreeSet::new(),
    };
    let mut claimed = BTreeSet::new();
    let mut order = Vec::with_capacity(plan.rules.len());
    for planned in &plan.rules {
        let existing = existing(&now, &planned.rule).filter(|index| claimed.insert(*index));
        order.push((planned.source, &planned.rule, existing));
    }
    // A rule the plan does not keep is not written: the filter it was read as stays.
    for (source, rule) in rules.iter().enumerate() {
        if write.outcomes[source].kept == Kept::NotKept
            && let Some(index) = existing(&now, rule).filter(|index| claimed.insert(*index))
        {
            write.unwritten(source, Some(index), refused(source));
        }
    }
    for (source, rule, existing) in order {
        write.one(source, &rules[source], rule, existing).await;
    }
    // Only now the filters no rule names: one claimed by a rule was replaced or kept above.
    for index in (0..now.filters.len()).filter(|index| !claimed.contains(index)) {
        write.delete(index).await;
    }
    Ok(RulesReceipt::new(None, write.outcomes).with_failures(write.failures))
}

/// The listing `base` was read from, or a refusal where it was not read: without its version and
/// raw form a write could not tell which filters to keep, and would remove every filter unseen.
fn refuse_unread(base: &RuleSet) -> ProviderResult<Listing> {
    let refuse = || {
        ProviderError::permanent(
            "a write of the mail rules needs them as they were read, version and raw form; \
             read them first",
        )
    };
    let version = base.version.as_ref().ok_or_else(refuse)?;
    let listing = base
        .raw
        .as_ref()
        .and_then(Listing::from_raw)
        .ok_or_else(refuse)?;
    if listing.version() != version.as_str() {
        return Err(refuse());
    }
    Ok(listing)
}

/// The index in `now` of the filter `rule` was read as, where it was read from this listing.
fn existing(now: &Listing, rule: &MailRule) -> Option<usize> {
    match rule.origin {
        Some(RuleOrigin::Listed { set, index })
            if set == now.set() && index < now.filters.len() =>
        {
            Some(index)
        }
        _ => None,
    }
}

struct Write<'a> {
    client: &'a GoogleClient,
    labels: &'a Labels,
    named: NamedLabels,
    now: &'a Listing,
    outcomes: Vec<RuleOutcome>,
    failures: Vec<RuleWriteFailure>,
    /// The addresses the account forwards to, once asked.
    forwarding: Option<BTreeSet<String>>,
    /// The filters read that stay: unchanged, or left in place of a rule not written.
    keep: BTreeSet<usize>,
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
        if let Some(index) = existing
            && self.now.rule(index, &self.named).as_ref() == Some(planned)
        {
            self.keep.insert(index);
            return;
        }
        let rule = match self.forwardable(source, given, planned).await {
            Ok(Some(rule)) => rule,
            Ok(None) => {
                if existing.is_some() {
                    self.unwritten(source, existing, refused(source));
                }
                return;
            }
            Err(failed) => return self.unwritten(source, existing, failed),
        };
        let Some(filter) = self.filter(source, &rule, existing).await else {
            return;
        };
        // A change the filter does not show (an `is` read back as `contains`) sends nothing: Gmail
        // refuses a filter identical to one it holds (observed).
        if let Some(index) = existing
            && same_filter(&self.now.filters[index], &filter)
        {
            self.keep.insert(index);
            return;
        }
        let url = self.client.url(FILTERS);
        let body = filter.to_string().into_bytes();
        match self.client.post(&url, "application/json", body).await {
            Ok(_) => {
                if let Some(index) = existing
                    && !self.delete(index).await
                {
                    // The old filter stays beside the new one: the outcome says so too.
                    self.outcomes[source].written_short(RulePart::Rule);
                }
            }
            Err(err) => self.unwritten(source, existing, failure(source, &err)),
        }
    }

    /// Records that the rule at `source` was not written, and keeps the filter at `existing`.
    fn unwritten(&mut self, source: usize, existing: Option<usize>, failed: RuleWriteFailure) {
        self.failures.push(failed);
        self.outcomes[source].not_written(RulePart::Rule);
        if let Some(index) = existing {
            self.keep.insert(index);
        }
    }

    /// `planned` without the forwards Gmail would refuse: to an address the account has not
    /// verified. Each is lost as `ForwardNotAllowed` on the action `given` held; `None` where
    /// nothing is left to do, and the failure where the addresses could not be asked.
    async fn forwardable(
        &mut self,
        source: usize,
        given: &MailRule,
        planned: &MailRule,
    ) -> Result<Option<MailRule>, RuleWriteFailure> {
        let mut rule = planned.clone();
        let RuleBody::Typed(written) = &mut rule.body else {
            return Ok(Some(rule));
        };
        if !written.actions.iter().any(forwards) {
            return Ok(Some(rule));
        }
        let verified = self.verified().await.map_err(|err| failure(source, &err))?;
        let positions = action_positions(given, planned);
        let mut kept = Vec::with_capacity(written.actions.len());
        for (k, action) in written.actions.drain(..).enumerate() {
            let refused = match &action {
                Action::Redirect { to, .. } | Action::Forward { to } => to
                    .iter()
                    .any(|address| !verified.contains(&address.as_str().to_lowercase())),
                _ => false,
            };
            if refused {
                let outcome = &mut self.outcomes[source];
                let part = positions
                    .get(k)
                    .map_or(RulePart::Rule, |j| RulePart::Action(*j));
                outcome
                    .losses
                    .push(Loss::new(part, LossReason::ForwardNotAllowed));
                if outcome.kept == Kept::Whole {
                    outcome.kept = Kept::Reduced;
                }
            } else {
                kept.push(action);
            }
        }
        if kept.is_empty() {
            self.outcomes[source].kept = Kept::NotKept;
            return Ok(None);
        }
        written.actions = kept;
        Ok(Some(rule))
    }

    /// The addresses Gmail has verified for forwarding, lowercased; asked once per write.
    async fn verified(&mut self) -> Result<BTreeSet<String>, GoogleError> {
        if let Some(verified) = &self.forwarding {
            return Ok(verified.clone());
        }
        let listed = self.client.get(&self.client.url(FORWARDING)).await?;
        let verified: BTreeSet<String> = listed
            .get("forwardingAddresses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|address| opt_str(address, "verificationStatus") == Some("accepted"))
            .filter_map(|address| opt_str(address, "forwardingEmail"))
            .map(str::to_lowercase)
            .collect();
        self.forwarding = Some(verified.clone());
        Ok(verified)
    }

    /// `rule` as a filter, or `None` where it is not written, the failure recorded: a keyword's
    /// label could not be found or made, or the writer cannot place a part.
    async fn filter(
        &mut self,
        source: usize,
        rule: &MailRule,
        existing: Option<usize>,
    ) -> Option<Value> {
        let mut label = BTreeMap::new();
        for keyword in named_keywords(rule) {
            match self.labels.label_id(self.client, &keyword).await {
                Some(Ok(id)) => {
                    label.insert(keyword, id);
                }
                Some(Err(err)) => {
                    self.unwritten(source, existing, failure(source, &err));
                    return None;
                }
                None => {}
            }
        }
        let filter = rules_write::filter(rule, &label).ok();
        if filter.is_none() {
            self.unwritten(source, existing, refused(source));
        }
        filter
    }

    /// `DELETE`s the filter at `index`; whether it is gone, one already gone included.
    async fn delete(&mut self, index: usize) -> bool {
        let Some(id) = opt_str(&self.now.filters[index], "id") else {
            self.failures.push(RuleWriteFailure::new(
                FailedRule::Removed(index),
                FailureClass::Permanent,
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

/// Whether `held` already says what `filter` does: the same criteria and the same actions.
fn same_filter(held: &Value, filter: &Value) -> bool {
    held.get("criteria") == filter.get("criteria") && held.get("action") == filter.get("action")
}

/// Whether `action` sends the message on: a forward Gmail checks.
fn forwards(action: &Action) -> bool {
    matches!(action, Action::Redirect { .. } | Action::Forward { .. })
}

/// For each action `planned` kept, its index among the actions `given` held: the plan keeps
/// actions in order and unchanged in an edit.
fn action_positions(given: &MailRule, planned: &MailRule) -> Vec<usize> {
    let (RuleBody::Typed(given), RuleBody::Typed(planned)) = (&given.body, &planned.body) else {
        return Vec::new();
    };
    let mut positions = Vec::with_capacity(planned.actions.len());
    let mut from = 0;
    for action in &planned.actions {
        if let Some(offset) = given.actions[from..].iter().position(|held| held == action) {
            positions.push(from + offset);
            from += offset + 1;
        }
    }
    positions
}

/// The keywords other than system ones that `rule` adds, each kept as a label.
fn named_keywords(rule: &MailRule) -> BTreeSet<Keyword> {
    let RuleBody::Typed(typed) = &rule.body else {
        return BTreeSet::new();
    };
    typed
        .actions
        .iter()
        .filter_map(|action| match action {
            Action::AddKeywords(keywords) => Some(keywords),
            _ => None,
        })
        .flatten()
        .filter(|keyword| keyword.as_system().is_none())
        .cloned()
        .collect()
}

/// A request's failure for the rule handed in at `source`, by class, and whether it may have
/// landed.
fn failure(source: usize, err: &GoogleError) -> RuleWriteFailure {
    with_outcome(
        RuleWriteFailure::new(FailedRule::Written(source), err.failure_class()),
        err,
    )
}

fn with_outcome(failed: RuleWriteFailure, err: &GoogleError) -> RuleWriteFailure {
    if err.leaves_outcome_unknown() {
        failed.outcome_unknown()
    } else {
        failed
    }
}

/// A rule this adapter does not send: it will not be stored as given, whatever is retried.
fn refused(source: usize) -> RuleWriteFailure {
    RuleWriteFailure::new(FailedRule::Written(source), FailureClass::Permanent)
}

#[cfg(test)]
#[path = "rules_apply_tests.rs"]
mod tests;
