//! What a write of the rules answers.

use super::{RuleOutcome, RulesActivation};
use crate::{error::FailureClass, version::RuleSetVersion};

/// What replacing an account's rules answers once the server has stored them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "the receipt says which rules the target did not keep"]
pub struct RulesReceipt {
    /// The version the rules have now, so the next write can be guarded without reading them
    /// again; `None` on a transport that keeps none, or that cannot say it yet because it applies
    /// a write after answering it (Graph): read them before the next write.
    pub version: Option<RuleSetVersion>,
    /// One outcome per rule handed in, as [`plan_rules`](super::plan_rules) decided it.
    pub outcomes: Vec<RuleOutcome>,
    /// Where the rules were stored.
    pub stored: RulesStored,
    /// What the write did to the account's out-of-office reply.
    pub reply: ReplyEffect,
    /// Whether the rules run now. A write never changes it
    /// ([`RuleSet::activation`](super::RuleSet::activation)): rules written into a script stored
    /// but switched off are stored, and do not run until the user switches it on. Said here so a
    /// transfer host's record of an import says it without a second read.
    pub activation: RulesActivation,
    /// The rules handed in, by index, that an import found the target already holding as they
    /// would be written, and did not write again: what makes running an import twice add nothing.
    /// Each still has its [`outcome`](Self::outcomes), as planned. Empty for an edit.
    pub already_held: Vec<usize>,
    /// Each rule a transport that stores rules one by one (Graph) did not store, or did not
    /// remove: the rest of the write went ahead, so these are reported here rather than failing
    /// it. Always empty where the set is stored at once (a Sieve script), which fails whole.
    pub failures: Vec<RuleWriteFailure>,
}

impl RulesReceipt {
    /// A receipt for rules written in place at `version`, each handed-in rule having had
    /// `outcomes`, the reply untouched, the rules running.
    pub fn new(version: Option<RuleSetVersion>, outcomes: Vec<RuleOutcome>) -> Self {
        Self {
            version,
            outcomes,
            stored: RulesStored::InPlace,
            reply: ReplyEffect::Untouched,
            activation: RulesActivation::Running,
            already_held: Vec::new(),
            failures: Vec::new(),
        }
    }

    /// Records the rules handed in that the target already held (builder-style).
    pub fn with_already_held(mut self, already_held: Vec<usize>) -> Self {
        self.already_held = already_held;
        self
    }

    /// Records whether the rules run now (builder-style).
    pub fn with_activation(mut self, activation: RulesActivation) -> Self {
        self.activation = activation;
        self
    }

    /// Records the rules the write did not store or remove (builder-style).
    pub fn with_failures(mut self, failures: Vec<RuleWriteFailure>) -> Self {
        self.failures = failures;
        self
    }

    /// Records where the rules were stored (builder-style).
    pub fn with_stored(mut self, stored: RulesStored) -> Self {
        self.stored = stored;
        self
    }

    /// Records what the write did to the reply (builder-style).
    pub fn with_reply(mut self, reply: ReplyEffect) -> Self {
        self.reply = reply;
        self
    }
}

/// Where a write stored the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RulesStored {
    /// Over the rules read, running or not as they were.
    InPlace,
    /// In a container the write created and switched on, because the account had no rules to
    /// edit (on Sieve, a new script named `rules`, made the active one).
    CreatedRunning,
    /// Nowhere: nothing was written, because no rule handed in was left to add (an import whose
    /// rules the target already held, or could keep none of).
    Unwritten,
}

/// What a write of the rules did to the account's out-of-office reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplyEffect {
    /// The reply runs as it ran, or there is none, or the transport keeps it apart.
    Untouched,
    /// A reply that ran before the write runs no longer: on a server that runs the reply as a
    /// script of its own, the write switched on another script and could not include the
    /// reply's in it. A host tells the user, and offers to turn the reply on again.
    Stopped,
}

/// One rule a write did not store or remove, by position and class: never its value, nor the
/// server's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RuleWriteFailure {
    /// Which rule.
    pub rule: FailedRule,
    /// Why, as the engine classes a failure: whether waiting, signing in again or reading again
    /// can help.
    pub class: FailureClass,
    /// Whether the request went out and no answer came back, so the change may have been made:
    /// read the rules again before writing again.
    pub outcome_unknown: bool,
}

impl RuleWriteFailure {
    /// `rule` was not stored or removed, for a reason of `class`.
    #[must_use]
    pub fn new(rule: FailedRule, class: FailureClass) -> Self {
        Self {
            rule,
            class,
            outcome_unknown: false,
        }
    }

    /// Records that the change may have been made after all (builder-style).
    #[must_use]
    pub fn outcome_unknown(mut self) -> Self {
        self.outcome_unknown = true;
        self
    }
}

/// A rule a write did not store or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailedRule {
    /// The rule at this index of the rules handed in was not stored as given: a new rule was not
    /// created, or an existing one is still as it was read (its order included).
    Written(usize),
    /// The rule at this index of the base was to be removed, and is still there.
    Removed(usize),
}
