//! [`RuleWriteEnding`]: where a rule-by-rule write of an account's rules stops sending.

use engine_core::rules::{FailedRule, RuleWriteFailure};

use crate::ProviderError;

/// What ended a rule-by-rule write of an account's rules, if anything.
///
/// A transport that stores rules one by one (Graph, Gmail) goes on past a rule the server
/// refuses, but not past a failure that reaches every later rule as well: a throttle, a
/// credential the server refused, or no credential at all. That is the decision an import's
/// batch makes ([`ProviderError::ends_batch`]), so a source that is down or hung is asked once
/// per write rather than once per rule at a deadline each. Every later request is then answered
/// without being sent, as a [`RuleWriteFailure`] of the same class, saying whether no
/// credential could be had.
///
/// # Examples
///
/// ```
/// use engine_core::{
///     error::FailureClass,
///     rules::{FailedRule, RuleWriteFailure},
/// };
/// use engine_provider::{ProviderError, RuleWriteEnding};
///
/// let mut ending = RuleWriteEnding::default();
/// assert!(ending.unsent(FailedRule::Written(1)).is_none());
/// let throttled = RuleWriteFailure::new(FailedRule::Written(0), FailureClass::RateLimited);
/// ending.note(throttled, &ProviderError::rate_limited("429", None));
/// let unsent = ending.unsent(FailedRule::Written(1)).expect("ended");
/// assert_eq!(unsent.class, FailureClass::RateLimited);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct RuleWriteEnding(Option<RuleWriteFailure>);

impl RuleWriteEnding {
    /// Records `failed`, the failure a request gave its rule, and ends the write where `error`,
    /// the same failure as the adapter classes it, [ends a batch](ProviderError::ends_batch).
    /// Returns `failed`, for the receipt.
    pub fn note(&mut self, failed: RuleWriteFailure, error: &ProviderError) -> RuleWriteFailure {
        if self.0.is_none() && error.ends_batch() {
            self.0 = Some(failed);
        }
        failed
    }

    /// Whether the write has ended: nothing more is sent, best-effort tidying included.
    #[must_use]
    pub fn has_ended(&self) -> bool {
        self.0.is_some()
    }

    /// For `rule`, whose request the write would send next: once the write has ended, the
    /// failure that answers it unsent (the ending's class and credential fact; never an unknown
    /// outcome, since nothing goes out). `None` while requests still go.
    #[must_use]
    pub fn unsent(&self, rule: FailedRule) -> Option<RuleWriteFailure> {
        self.0.map(|ended| {
            let unsent = RuleWriteFailure::new(rule, ended.class);
            if ended.credential_failure {
                unsent.credential_failure()
            } else {
                unsent
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use engine_core::error::FailureClass;

    use super::*;

    #[test]
    fn a_write_ends_on_what_ends_a_batch_and_answers_the_rest_unsent() {
        let mut ending = RuleWriteEnding::default();
        // A failure about one rule ends nothing.
        let refused = RuleWriteFailure::new(FailedRule::Written(0), FailureClass::Permanent);
        ending.note(refused, &ProviderError::permanent("400"));
        assert!(ending.unsent(FailedRule::Written(1)).is_none());
        assert!(!ending.has_ended());

        let no_credential = RuleWriteFailure::new(FailedRule::Written(1), FailureClass::Retryable)
            .credential_failure();
        ending.note(
            no_credential,
            &ProviderError::retryable("the broker is down").of_credential(),
        );
        // A later failure does not replace the ending.
        let throttled = RuleWriteFailure::new(FailedRule::Written(2), FailureClass::RateLimited);
        ending.note(throttled, &ProviderError::rate_limited("429", None));
        assert!(ending.has_ended());
        let unsent = ending.unsent(FailedRule::Removed(4)).expect("ended");
        assert_eq!(unsent.rule, FailedRule::Removed(4));
        assert_eq!(unsent.class, FailureClass::Retryable);
        assert!(unsent.credential_failure && !unsent.outcome_unknown);
    }
}
