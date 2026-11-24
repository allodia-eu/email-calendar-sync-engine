//! Replacing the account's rules over Graph, one `messageRule` at a time
//! (`filters-and-out-of-office.md`, §4: "Graph and Gmail adapters diff `base` against `rules`").
//!
//! 1. **Planned** by `plan_rules` in edit mode against [`vocabulary`]; what the writer still cannot
//!    place (`crate::rules_write`) is not kept either, and the receipt says which part.
//! 2. **Refused before sending** where `base` was not read (no version, or a raw form that is not
//!    the one its version names: `Permanent`), or where the listing moved since (`Conflict`).
//! 3. **Written in order.** Each written rule is the `messageRule` its origin names in `base`, or a
//!    new one: a new rule is `POST`ed with its place as `sequence`; one that changed is `PATCH`ed
//!    whole, every predicate and action it no longer has cleared; one whose place moved, or that
//!    follows a rule whose place moved, has its `sequence` `PATCH`ed. In order, so the order holds
//!    whether Exchange renumbers the others on each change or keeps the numbers given. An unchanged
//!    rule in its place is not sent at all.
//! 4. **Then removed**: every rule of `base` no written rule names is `DELETE`d, after the writes,
//!    so a failed write never leaves the account with neither the old rule nor the new; one already
//!    gone is the state asked for.
//! 5. **Read-only rules are never sent**: one the caller changed, moved or removed is reported as a
//!    `Permanent` failure.
//!
//! Each request's failure is a [`RuleWriteFailure`] in the receipt, by position and class, and
//! the rest go ahead; the receipt's version is the listing read again afterwards.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    error::FailureClass,
    ids::MailboxId,
    mail::MailboxRole,
    rules::{
        FailedRule, Kept, Loss, LossReason, MailRule, PlanMode, RuleOrigin, RuleOutcome, RuleSet,
        RuleWriteFailure, RulesReceipt, plan_rules,
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
        disturbed: false,
    };
    let mut kept = BTreeSet::new();
    for (place, planned) in plan.rules.iter().enumerate() {
        let existing = existing(&now, &planned.rule).filter(|index| !kept.contains(index));
        let placed = write
            .one(planned.source, &planned.rule, existing, place + 1)
            .await;
        // A rule the writer could not place is not in the set written, as on every transport,
        // so the one it was read as goes; one whose request failed is still there as it was.
        if let (Body::Placed(()), Some(index)) = (placed, existing) {
            kept.insert(index);
        }
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
    /// Whether a rule before this one was created or moved, so every later one is placed again.
    disturbed: bool,
}

/// What became of one rule: placed (written, or tried and failed, so its old self stays), or not
/// placed by the writer (not kept).
enum Body<T> {
    Placed(T),
    Failed,
    NotPlaced,
}

impl Write<'_> {
    /// Writes one rule in its place.
    async fn one(
        &mut self,
        source: usize,
        rule: &MailRule,
        existing: Option<usize>,
        place: usize,
    ) -> Body<()> {
        let failed = |class| RuleWriteFailure::new(FailedRule::Written(source), class);
        let Some(index) = existing else {
            let mut body = match self.body(source, rule).await {
                Body::Placed(body) => body,
                Body::Failed => return Body::Failed,
                Body::NotPlaced => return Body::NotPlaced,
            };
            body.insert("sequence".to_owned(), json!(place));
            let url = self.client.url(RULES);
            if let Err(err) = self
                .client
                .post(&url, "application/json", encode(&body))
                .await
            {
                self.failures
                    .push(failure(FailedRule::Written(source), &err));
            }
            self.disturbed = true;
            return Body::Placed(());
        };
        let old = &self.now.rules[index];
        let unchanged = self.now.rule(index).as_ref() == Some(rule);
        let numbered = old.get("sequence").and_then(Value::as_u64) == u64::try_from(place).ok();
        let moved = self.disturbed || !numbered;
        if unchanged && !moved {
            return Body::Placed(());
        }
        if read_only(old) {
            // Never sent. Its number may still be right after a rule before it was placed.
            if !unchanged || !numbered {
                self.failures.push(failed(FailureClass::Permanent));
            }
            return Body::Placed(());
        }
        let mut patch = Map::new();
        if !unchanged {
            patch = match self.body(source, rule).await {
                Body::Placed(body) => body,
                Body::Failed => return Body::Placed(()),
                Body::NotPlaced => return Body::NotPlaced,
            };
            rules_write::clear_dropped(&mut patch, old);
        }
        if moved {
            patch.insert("sequence".to_owned(), json!(place));
            self.disturbed = true;
        }
        let Some(url) = self.rule_url(index) else {
            self.failures.push(failed(FailureClass::Permanent));
            return Body::Placed(());
        };
        if let Err(err) = self
            .client
            .patch(&url, "application/json", None, encode(&patch))
            .await
        {
            self.failures
                .push(failure(FailedRule::Written(source), &err));
        }
        Body::Placed(())
    }

    async fn remove(&mut self, index: usize) {
        let refused = RuleWriteFailure::new(FailedRule::Removed(index), FailureClass::Permanent);
        let url = match self.rule_url(index) {
            Some(url) if !read_only(&self.now.rules[index]) => url,
            _ => {
                self.failures.push(refused);
                return;
            }
        };
        match self.client.delete(&url, None).await {
            Err(err) if !is_not_found(&err) => {
                self.failures
                    .push(failure(FailedRule::Removed(index), &err));
            }
            _ => {}
        }
    }

    /// The rule's own URL, or `None` where its id cannot be one path segment: Graph decodes an
    /// encoded `/` and parses it as a separator (`crate::principal`), so such an id is not sent.
    fn rule_url(&self, index: usize) -> Option<String> {
        let id =
            opt_str(&self.now.rules[index], "id").filter(|id| !id.contains(['/', '?', '#']))?;
        Some(self.client.url(&format!("{RULES}/{id}")))
    }

    /// `rule`'s body, or why it is not written: the writer cannot place a part (not kept, that
    /// part named), or a folder it names by role could not be looked up (a failure).
    async fn body(&mut self, source: usize, rule: &MailRule) -> Body<Map<String, Value>> {
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
                        self.failures
                            .push(failure(FailedRule::Written(source), &err));
                        return Body::Failed;
                    }
                },
            };
            self.folders.insert(role, found);
        }
        let folders = &self.folders;
        match rules_write::body(rule, &|role| folders.get(role).cloned().flatten()) {
            Ok(body) => Body::Placed(body),
            Err(part) => {
                let outcome = &mut self.outcomes[source];
                outcome.kept = Kept::NotKept;
                outcome
                    .losses
                    .push(Loss::new(part, LossReason::NotExpressible));
                Body::NotPlaced
            }
        }
    }
}

fn read_only(rule: &Value) -> bool {
    rule.get("isReadOnly").and_then(Value::as_bool) == Some(true)
}

/// A request's failure, by class, and whether it may have landed.
fn failure(rule: FailedRule, err: &GraphError) -> RuleWriteFailure {
    let failed = RuleWriteFailure::new(rule, err.failure_class());
    if err.leaves_outcome_unknown() {
        failed.outcome_unknown()
    } else {
        failed
    }
}

fn encode(body: &Map<String, Value>) -> Vec<u8> {
    // A map of JSON values always serializes.
    serde_json::to_vec(body).unwrap_or_default()
}

#[cfg(test)]
#[path = "rules_apply_tests.rs"]
mod tests;
