//! Replacing the account's rules over Graph, one `messageRule` at a time
//! (`filters-and-out-of-office.md`, §4: "Graph and Gmail adapters diff `base` against `rules`").
//!
//! 1. **Planned** by `plan_rules` in edit mode against [`vocabulary`], whose features and limits
//!    say everything the writer (`crate::rules_write`) cannot place. The set written is the set
//!    planned, as on every transport: a rule the plan does not keep is not there afterwards, so the
//!    rule it was read as is removed with the others left out.
//! 2. **Refused before sending** where `base` was not read (no version, or a raw form that is not
//!    the one its version names: `Permanent`), or where the listing moved since (`Conflict`).
//! 3. **Written in order.** Each written rule is the `messageRule` its origin names in `base`, or a
//!    new one: a new rule is `POST`ed in its place; one that changed is `PATCH`ed whole, every
//!    predicate and action it no longer has cleared and its recipients' names kept. Places are
//!    numbers that only rise through the rules written: a rule keeps its `sequence` while it is
//!    above the last one placed and nothing before it moved, and otherwise takes the next number,
//!    so the order holds whether Exchange renumbers the others on each change or keeps the numbers
//!    given. An unchanged rule in its place is not sent at all.
//! 4. **Then removed**: every rule of `base` no written rule names is `DELETE`d, after the writes,
//!    so a failed write never leaves the account with neither the old rule nor the new; one already
//!    gone is the state asked for.
//! 5. **Read-only rules are never sent.** One whose number still falls in order is in its place;
//!    one the caller changed, moved out of order or removed is a `Permanent` failure.
//!
//! Each request's failure is a [`RuleWriteFailure`] in the receipt, by position and class, and the
//! rest go ahead. The rule's outcome says so too ([`LossReason::NotWritten`]): not kept where the
//! rule itself was not stored, reduced where only its place was not, so no outcome reads whole for
//! a rule the receipt lists as failed. A rule the writer cannot place despite the plan is a bug
//! handled the same way: the server's rule stays as it was. The receipt's version is the listing
//! read again afterwards.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::MailboxRole,
    rules::{
        FailedRule, Kept, Loss, LossReason, MailRule, PlanMode, RuleOrigin, RuleOutcome, RulePart,
        RuleSet, RuleWriteFailure, RulesReceipt, plan_rules,
    },
    version::RuleSetVersion,
};
use engine_provider::{ProviderError, ProviderResult};
use serde_json::{Map, Value, json};

use crate::{
    error::GraphError,
    fetch::well_known_id,
    json::opt_str,
    mailbox_write::is_not_found,
    rules::{Listing, RULES, vocabulary},
    rules_write::{self, roles_needed, well_known},
    transport::GraphClient,
};

pub(crate) async fn replace(
    client: &GraphClient,
    base: &RuleSet,
    rules: &[MailRule],
) -> ProviderResult<RulesReceipt> {
    let plan = plan_rules(rules, &vocabulary(), PlanMode::Edit);
    let read = refuse_unread(base)?;
    let now = Listing::read(client).await?;
    if now.version() != read.version() {
        return Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ));
    }
    let mut write = Write {
        client,
        now: &now,
        outcomes: plan.outcomes,
        failures: Vec::new(),
        folders: BTreeMap::new(),
        last: 0,
        disturbed: false,
    };
    let mut kept = BTreeSet::new();
    for planned in &plan.rules {
        let existing = existing(&now, &planned.rule).filter(|index| kept.insert(*index));
        write.one(planned.source, &planned.rule, existing).await;
    }
    for index in (0..now.rules.len()).filter(|index| !kept.contains(index)) {
        write.remove(index).await;
    }
    let version = Listing::read(client)
        .await
        .ok()
        .map(|after| RuleSetVersion::new(after.version()));
    Ok(RulesReceipt::new(version, write.outcomes).with_failures(write.failures))
}

/// The listing `base` was read from, or a refusal where it was not read: without its version and
/// raw form a write could not tell which rules to keep, and would remove every rule unseen.
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

/// The index in `now` of the rule `rule` was read as, where it was read from this listing.
fn existing(now: &Listing, rule: &MailRule) -> Option<usize> {
    match rule.origin {
        Some(RuleOrigin::Listed { set, index }) if set == now.set() && index < now.rules.len() => {
            Some(index)
        }
        _ => None,
    }
}

struct Write<'a> {
    client: &'a GraphClient,
    now: &'a Listing,
    outcomes: Vec<RuleOutcome>,
    failures: Vec<RuleWriteFailure>,
    /// The folder each role resolved to; `None` where the account has none.
    folders: BTreeMap<MailboxRole, Option<MailboxId>>,
    /// The `sequence` of the last rule placed: every later one is placed above it.
    last: u64,
    /// Whether a rule was created or moved, so every later one is placed again: where Exchange
    /// renumbers the others on a change, the numbers read are stale from there on.
    disturbed: bool,
}

/// What part of a rule a failure cost it: the rule as given, or only its place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cost {
    Rule,
    Place,
}

impl Write<'_> {
    /// Writes one planned rule: the rule at `existing` in the listing, or a new one.
    async fn one(&mut self, source: usize, rule: &MailRule, existing: Option<usize>) {
        let Some(index) = existing else {
            let Some(mut body) = self.body(source, rule).await else {
                return;
            };
            let place = self.last + 1;
            body.insert("sequence".to_owned(), json!(place));
            let url = self.client.url(RULES);
            match self
                .client
                .post(&url, "application/json", encode(&body))
                .await
            {
                Ok(_) => {
                    self.last = place;
                    self.disturbed = true;
                }
                Err(err) => self.failed(source, Cost::Rule, failure(source, &err)),
            }
            return;
        };
        let old = &self.now.rules[index];
        let current = old.get("sequence").and_then(Value::as_u64);
        let unchanged = self.now.rule(index).as_ref() == Some(rule);
        if read_only(old) {
            // Never sent. In its place where its number still falls in order.
            let in_order = current.is_some_and(|number| number > self.last);
            if !unchanged {
                self.failed(source, Cost::Rule, refused(source));
            } else if !in_order {
                self.failed(source, Cost::Place, refused(source));
            }
            if let Some(number) = current.filter(|_| in_order) {
                self.last = number;
            }
            return;
        }
        let place = match current {
            Some(number) if !self.disturbed && number > self.last => number,
            _ => self.last + 1,
        };
        let mut patch = Map::new();
        if !unchanged {
            let Some(body) = self.body(source, rule).await else {
                return;
            };
            patch = body;
            rules_write::clear_dropped(&mut patch, old);
            rules_write::keep_names(&mut patch, old);
        }
        let moved = current != Some(place);
        if moved {
            patch.insert("sequence".to_owned(), json!(place));
        }
        if patch.is_empty() {
            self.last = place;
            return;
        }
        let url = self.rule_url(index);
        match self
            .client
            .patch(&url, "application/json", None, encode(&patch))
            .await
        {
            Ok(_) => {
                self.last = place;
                self.disturbed |= moved;
            }
            Err(err) => {
                let cost = if unchanged { Cost::Place } else { Cost::Rule };
                self.failed(source, cost, failure(source, &err));
            }
        }
    }

    /// Records `failure`, and on the rule's outcome what it cost.
    fn failed(&mut self, source: usize, cost: Cost, failure: RuleWriteFailure) {
        self.failures.push(failure);
        let outcome = &mut self.outcomes[source];
        let part = match cost {
            Cost::Rule => RulePart::Rule,
            Cost::Place => RulePart::Order,
        };
        if outcome
            .losses
            .iter()
            .any(|loss| loss.part == part && loss.reason == LossReason::NotWritten)
        {
            return;
        }
        outcome.losses.push(Loss::new(part, LossReason::NotWritten));
        outcome.kept = match (cost, outcome.kept) {
            (Cost::Rule, _) => Kept::NotKept,
            (Cost::Place, Kept::Whole) => Kept::Reduced,
            (Cost::Place, kept) => kept,
        };
    }

    async fn remove(&mut self, index: usize) {
        if read_only(&self.now.rules[index]) {
            let refused =
                RuleWriteFailure::new(FailedRule::Removed(index), FailureClass::Permanent);
            self.failures.push(refused);
            return;
        }
        let url = self.rule_url(index);
        match self.client.delete(&url, None).await {
            Err(err) if !is_not_found(&err) => {
                let failed = with_outcome(
                    RuleWriteFailure::new(FailedRule::Removed(index), err.failure_class()),
                    &err,
                );
                self.failures.push(failed);
            }
            _ => {}
        }
    }

    /// The rule's own URL, its id one percent-encoded path segment.
    fn rule_url(&self, index: usize) -> String {
        let id = opt_str(&self.now.rules[index], "id").unwrap_or_default();
        self.client.url(&format!("{RULES}/{}", encode_id(id)))
    }

    /// `rule`'s body, or `None` where it is not written, the failure recorded: a folder it names
    /// by role could not be looked up, or the writer cannot place a part the plan let through (a
    /// bug: the server's rule stays as it was).
    async fn body(&mut self, source: usize, rule: &MailRule) -> Option<Map<String, Value>> {
        for role in roles_needed(rule) {
            if self.folders.contains_key(&role) {
                continue;
            }
            let found = match well_known(&role) {
                None => None,
                Some(alias) => match well_known_id(self.client, alias).await {
                    Ok(id) => Some(id),
                    Err(err) if is_not_found(&err) => None,
                    Err(err) => {
                        self.failed(source, Cost::Rule, failure(source, &err));
                        return None;
                    }
                },
            };
            self.folders.insert(role, found);
        }
        let folders = &self.folders;
        let body = rules_write::body(rule, &|role| folders.get(role).cloned().flatten()).ok();
        if body.is_none() {
            self.failed(source, Cost::Rule, refused(source));
        }
        body
    }
}

fn read_only(rule: &Value) -> bool {
    rule.get("isReadOnly").and_then(Value::as_bool) == Some(true)
}

/// A request's failure for the rule handed in at `source`, by class, and whether it may have
/// landed.
fn failure(source: usize, err: &GraphError) -> RuleWriteFailure {
    with_outcome(
        RuleWriteFailure::new(FailedRule::Written(source), err.failure_class()),
        err,
    )
}

fn with_outcome(failed: RuleWriteFailure, err: &GraphError) -> RuleWriteFailure {
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

/// A rule id as one path segment: every byte but RFC 3986's unreserved ones percent-encoded, so a
/// `/`, `+` or `=` in a base64 id is data.
fn encode_id(id: &str) -> String {
    use core::fmt::Write as _;

    id.bytes()
        .fold(String::with_capacity(id.len()), |mut out, byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                out.push(char::from(byte));
            } else {
                let _ = write!(out, "%{byte:02X}");
            }
            out
        })
}

fn encode(body: &Map<String, Value>) -> Vec<u8> {
    // A map of JSON values always serializes.
    serde_json::to_vec(body).unwrap_or_default()
}

#[cfg(test)]
#[path = "rules_apply_tests.rs"]
mod tests;
