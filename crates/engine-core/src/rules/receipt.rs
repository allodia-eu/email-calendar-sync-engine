//! What a write of the rules answers.

use super::RuleOutcome;
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
}

impl RulesReceipt {
    /// A receipt for rules stored at `version`, each handed-in rule having had `outcomes`.
    pub fn new(version: Option<RuleSetVersion>, outcomes: Vec<RuleOutcome>) -> Self {
        Self { version, outcomes }
    }
}
