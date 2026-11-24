//! Replacing the account's rules over Graph, one `messageRule` at a time
//! (`filters-and-out-of-office.md`, §4: "Graph and Gmail adapters diff `base` against `rules`").
//!
//! 1. **Planned** by `plan_rules` in edit mode against [`vocabulary`], whose features and limits
//!    say everything the writer (`crate::rules_write`) cannot place. The set written is the set
//!    planned, as on every transport: a rule the plan does not keep is not there afterwards, so the
//!    rule it was read as is removed with the others left out.
//! 2. **Refused before sending** where `base` was not read (no version, or a raw form that is not
//!    the one its version names: `Permanent`), or where the listing moved since (`Conflict`).
//!    Graph's listing can lag a write it has just answered (observed: a rule `POST`ed a moment
//!    earlier was not yet listed), so the check narrows the race and does not close it.
//! 3. **Removed first**: every rule of `base` no written rule names is `DELETE`d (one already gone
//!    is the state asked for), so the positions below are those of the rules that stay.
//! 4. **Written in order, by position.** Graph keeps the rules as a list: `sequence` is a rule's
//!    position, renumbered on every change, and a number past the end is clamped to it (observed).
//!    So the writer keeps the server's list as it goes and sends only what moves it: a new rule is
//!    `POST`ed at the position it takes; a rule elsewhere than its position gets a `PATCH` of its
//!    `sequence` alone; a rule in place is not sent. Inserting or moving one rule moves no other.
//! 5. **Changed in place where Graph can.** A changed rule is `PATCH`ed whole, its recipients'
//!    names kept. Graph does not clear a part on `PATCH`: a predicate, an action or the name sent
//!    as `null` stays as it was (observed). So a change that drops a part is written as a new rule
//!    in its place, `POST`ed first, and the old one is `DELETE`d after it; a rule given no name
//!    gets the name Graph chooses for it (observed).
//! 6. **Read-only rules are never sent.** One whose position is still right is in its place; one
//!    the caller changed, moved out of place or removed is a `Permanent` failure.
//!
//! Each request's failure is a [`RuleWriteFailure`] in the receipt, by position and class, and the
//! rest go ahead. The rule's outcome says so too ([`LossReason::NotWritten`]): not kept where the
//! rule itself was not stored, reduced where only its place was not, so no outcome reads whole for
//! a rule the receipt lists as failed. A rule the writer cannot place despite the plan is a bug
//! handled the same way: the server's rule stays as it was. The receipt carries no version: the
//! listing can lag the writes just made, so the next write reads first.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::MailboxRole,
    rules::{
        FailedRule, Kept, Loss, LossReason, MailRule, PlanMode, RuleOrigin, RuleOutcome, RulePart,
        RuleSet, RuleWriteFailure, RulesReceipt, plan_rules,
    },
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
    let mut keep = BTreeSet::new();
    let mut order = Vec::with_capacity(plan.rules.len());
    for planned in &plan.rules {
        let existing = existing(&now, &planned.rule).filter(|index| keep.insert(*index));
        order.push((planned.source, &planned.rule, existing));
    }
    let mut write = Write {
        client,
        now: &now,
        outcomes: plan.outcomes,
        failures: Vec::new(),
        folders: BTreeMap::new(),
        live: (0..now.rules.len()).map(Slot::Read).collect(),
        placed: 0,
        pending: keep.clone(),
    };
    for index in (0..now.rules.len()).filter(|index| !keep.contains(index)) {
        write.remove(index).await;
    }
    for (source, rule, existing) in order {
        write.one(source, rule, existing).await;
    }
    // The listing can lag the writes just answered (observed), so a read now names no version a
    // later write could trust: the receipt carries none, and the next write reads first.
    Ok(RulesReceipt::new(None, write.outcomes).with_failures(write.failures))
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
    /// The server's list as the writes leave it, in position order.
    live: Vec<Slot>,
    /// How many of `live`'s first entries are final: the rules written so far, and any the
    /// writer could not move or remove among them.
    placed: usize,
    /// The rules read that are still to be written.
    pending: BTreeSet<usize>,
}

/// One entry of the server's list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The rule at this index of the listing read.
    Read(usize),
    /// A rule this write created.
    Created,
}

/// What part of a rule a failure cost it: the rule as given, or only its place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cost {
    Rule,
    Place,
}

impl Write<'_> {
    /// Writes one planned rule at the next position: the rule at `existing` in the listing, or a
    /// new one.
    async fn one(&mut self, source: usize, rule: &MailRule, existing: Option<usize>) {
        self.skip_unplaced();
        let Some(index) = existing else {
            if let Some(body) = self.body(source, rule, None).await
                && self.create(source, body).await
            {
                self.live.insert(self.placed, Slot::Created);
                self.placed += 1;
            }
            return;
        };
        self.pending.remove(&index);
        let Some(at) = self.live.iter().position(|slot| *slot == Slot::Read(index)) else {
            return;
        };
        let old = &self.now.rules[index];
        let unchanged = self.now.rule(index).as_ref() == Some(rule);
        if read_only(old) {
            // Never sent. In its place where its position is already right.
            if !unchanged {
                self.failed(source, Cost::Rule, refused(source));
            } else if at != self.placed {
                self.failed(source, Cost::Place, refused(source));
            }
            if at == self.placed {
                self.placed += 1;
            }
            return;
        }
        let mut patch = Map::new();
        if !unchanged {
            let Some(body) = self.body(source, rule, Some(old)).await else {
                if at == self.placed {
                    self.placed += 1;
                }
                return;
            };
            if rules_write::drops(&body, old) {
                // Graph keeps on a `PATCH` what it is asked to clear: a new rule in its place.
                if self.create(source, body).await {
                    self.live.insert(self.placed, Slot::Created);
                    self.placed += 1;
                    let at = at + 1;
                    if self.delete(FailedRule::Removed(index), index).await {
                        self.live.remove(at);
                    }
                }
                return;
            }
            patch = body;
        }
        let moved = at != self.placed;
        if moved {
            patch.insert("sequence".to_owned(), json!(self.placed + 1));
        }
        if patch.is_empty() {
            self.placed += 1;
            return;
        }
        let url = self.rule_url(index);
        match self
            .client
            .patch(&url, "application/json", None, encode(&patch))
            .await
        {
            Ok(_) => {
                let slot = self.live.remove(at);
                self.live.insert(self.placed, slot);
                self.placed += 1;
            }
            Err(err) => {
                let cost = if unchanged { Cost::Place } else { Cost::Rule };
                self.failed(source, cost, failure(source, &err));
                if !moved {
                    self.placed += 1;
                }
            }
        }
    }

    /// Steps over the entries at the next position that no written rule will take: a read-only
    /// rule left out, a rule whose removal failed, a rule left out of place by a failed move.
    fn skip_unplaced(&mut self) {
        // Every rule still to be written is found later in the list; what sits at the next
        // position and is not one of them stays where it is.
        while let Some(Slot::Read(index)) = self.live.get(self.placed).copied() {
            if self.pending.contains(&index) {
                break;
            }
            self.placed += 1;
        }
    }

    /// `POST`s a new rule at the next position; whether Graph took it.
    async fn create(&mut self, source: usize, mut body: Map<String, Value>) -> bool {
        body.insert("sequence".to_owned(), json!(self.placed + 1));
        let url = self.client.url(RULES);
        match self
            .client
            .post(&url, "application/json", encode(&body))
            .await
        {
            Ok(_) => true,
            Err(err) => {
                self.failed(source, Cost::Rule, failure(source, &err));
                false
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

    /// Removes a rule left out, unless it is read-only.
    async fn remove(&mut self, index: usize) {
        if read_only(&self.now.rules[index]) {
            let refused =
                RuleWriteFailure::new(FailedRule::Removed(index), FailureClass::Permanent);
            self.failures.push(refused);
            return;
        }
        if self.delete(FailedRule::Removed(index), index).await {
            self.live.retain(|slot| *slot != Slot::Read(index));
        }
    }

    /// `DELETE`s the rule at `index`; whether it is gone, one already gone included.
    async fn delete(&mut self, rule: FailedRule, index: usize) -> bool {
        let url = self.rule_url(index);
        match self.client.delete(&url, None).await {
            Err(err) if !is_not_found(&err) => {
                let failed = with_outcome(RuleWriteFailure::new(rule, err.failure_class()), &err);
                self.failures.push(failed);
                false
            }
            _ => true,
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
    async fn body(
        &mut self,
        source: usize,
        rule: &MailRule,
        old: Option<&Value>,
    ) -> Option<Map<String, Value>> {
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
        let mut body = rules_write::body(rule, &|role| folders.get(role).cloned().flatten()).ok();
        match (&mut body, old) {
            (None, _) => self.failed(source, Cost::Rule, refused(source)),
            (Some(body), Some(old)) => rules_write::keep_names(body, old),
            (Some(_), None) => {}
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
