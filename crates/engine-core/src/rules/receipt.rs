//! What a write of the rules answers.

use super::{RuleOutcome, RulesActivation};
use crate::version::RuleSetVersion;

/// What replacing an account's rules answers once the server has stored them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "the receipt says which rules the target did not keep"]
pub struct RulesReceipt {
    /// The version the rules have now, so the next write can be guarded without reading them
    /// again; `None` on a transport that keeps none.
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
        }
    }

    /// Records whether the rules run now (builder-style).
    pub fn with_activation(mut self, activation: RulesActivation) -> Self {
        self.activation = activation;
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
