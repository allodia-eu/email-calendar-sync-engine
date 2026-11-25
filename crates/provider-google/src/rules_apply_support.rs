//! What the filters' writer (`crate::rules_apply`) asks along the way: whether a forward may be
//! written, where Gmail already holds a filter, and how a failure is recorded.

use std::collections::BTreeSet;

use engine_core::{
    error::FailureClass,
    mail::Keyword,
    rules::{
        Action, FailedRule, Kept, Loss, LossReason, MailRule, NativeKind, RuleBody, RulePart,
        RuleWriteFailure,
    },
};
use serde_json::Value;

use crate::{error::GoogleError, json::opt_str, rules_apply::Write};

/// The account's forwarding addresses.
const FORWARDING: &str = "/gmail/v1/users/me/settings/forwardingAddresses";

/// Where Gmail already holds a filter a rule asks for.
pub(crate) enum Held {
    /// It is the filter the rule was read as: nothing to send or remove.
    AsRead,
    /// It is a filter read that no rule claims: the rule takes it.
    Unclaimed(usize),
    /// A filter another rule keeps as it is, or this write created, says it already.
    Kept,
    /// Nowhere: it is created.
    Nowhere,
}

/// Whether `held` already says what `filter` does: the same criteria and the same actions.
pub(crate) fn same_filter(held: &Value, filter: &Value) -> bool {
    held.get("criteria") == filter.get("criteria") && held.get("action") == filter.get("action")
}

/// The address a forward of `action` sends to, typed or Gmail's own (`{"forward": …}`).
fn forward_address(action: &Action) -> Option<Vec<String>> {
    match action {
        Action::Redirect { to, .. } | Action::Forward { to } => {
            Some(to.iter().map(|a| a.as_str().to_owned()).collect())
        }
        Action::Native(native) if native.kind == NativeKind::Gmail => {
            let fragment: Value = serde_json::from_str(&native.raw).ok()?;
            Some(vec![opt_str(&fragment, "forward")?.to_owned()])
        }
        _ => None,
    }
}

impl Write<'_> {
    /// `planned` without the forwards Gmail would refuse: to an address the account has not
    /// verified, typed or Gmail's own. Each is lost as `ForwardNotAllowed` on the action `given`
    /// held; `None` where nothing is left to do, and the failure where the addresses could not
    /// be asked.
    pub(crate) async fn forwardable(
        &mut self,
        source: usize,
        given: &MailRule,
        planned: &MailRule,
    ) -> Result<Option<MailRule>, RuleWriteFailure> {
        let mut rule = planned.clone();
        let RuleBody::Typed(written) = &mut rule.body else {
            return Ok(Some(rule));
        };
        if !written.actions.iter().any(|a| forward_address(a).is_some()) {
            return Ok(Some(rule));
        }
        if self.forwarding.is_none()
            && let Some(unsent) = self.ending.unsent(FailedRule::Written(source))
        {
            // Reading the addresses would be a request, which an ended write no longer sends.
            return Err(unsent);
        }
        let verified = match self.verified().await {
            Ok(verified) => verified,
            Err(err) => return Err(self.ended_by(failure(source, &err), err)),
        };
        let positions = action_positions(given, planned);
        let mut kept = Vec::with_capacity(written.actions.len());
        for (k, action) in written.actions.drain(..).enumerate() {
            let refused = forward_address(&action).is_some_and(|to| {
                to.iter()
                    .any(|address| !verified.contains(&address.to_lowercase()))
            });
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

    /// `failed`, the failure `err` gave its rule, after noting whether it ends the write: a
    /// failure that reaches every later rule
    /// ([`ProviderError::ends_batch`](engine_provider::ProviderError::ends_batch)) stops every
    /// later request.
    pub(crate) fn ended_by(
        &mut self,
        failed: RuleWriteFailure,
        err: GoogleError,
    ) -> RuleWriteFailure {
        self.ending
            .note(failed, &engine_provider::ProviderError::from(err))
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
pub(crate) fn named_keywords(rule: &MailRule) -> BTreeSet<Keyword> {
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
pub(crate) fn failure(source: usize, err: &GoogleError) -> RuleWriteFailure {
    with_outcome(
        RuleWriteFailure::new(FailedRule::Written(source), err.failure_class()),
        err,
    )
}

/// `failed`, with what `err` says beyond its class: whether the change may have landed, and
/// whether no credential could be had for it.
pub(crate) fn with_outcome(failed: RuleWriteFailure, err: &GoogleError) -> RuleWriteFailure {
    let failed = if err.leaves_outcome_unknown() {
        failed.outcome_unknown()
    } else {
        failed
    };
    match err {
        GoogleError::Credential(source) if source.is_credential_failure() => {
            failed.credential_failure()
        }
        _ => failed,
    }
}

/// A rule this adapter does not send: it will not be stored as given, whatever is retried.
pub(crate) fn refused(source: usize) -> RuleWriteFailure {
    RuleWriteFailure::new(FailedRule::Written(source), FailureClass::Permanent)
}
