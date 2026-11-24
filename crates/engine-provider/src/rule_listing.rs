//! What a transport that keeps an account's rules as separate objects (Graph's `messageRule`s,
//! Gmail's filters) answers the same way whichever it is: one answer to each question, shared.
//!
//! Such a transport reads its rules as one listing, kept whole as the set's raw form. Its version
//! is a SHA-256 of that text, a rule read from it names its position there
//! ([`RuleOrigin::Listed`]), and a write is built on a base read from the account: one with no
//! version or raw form, or a raw form its version does not name, is refused before anything is
//! sent, since the write could not tell which rules to keep and would remove every rule unseen.

use engine_core::{
    error::FailureClass,
    raw::RawRules,
    rules::{FailedRule, Kept, MailRule, RuleOrigin, RuleOutcome, RuleSet, RuleWriteFailure},
    version::RuleSetVersion,
};
use sha2::{Digest, Sha256};

use crate::{ProviderError, ProviderResult};

/// A listing of rules as one text: the provider's own form, in the order its adapter keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleListing {
    text: String,
}

impl RuleListing {
    /// The listing kept as `text`, in the adapter's order: an order the provider changes without
    /// a rule changing must not reach the text, or a write would see a conflict nothing caused.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The text, as kept.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text as the set's raw form.
    #[must_use]
    pub fn raw(&self) -> RawRules {
        RawRules::new(self.text.clone())
    }

    fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.text.as_bytes()).into()
    }

    /// The version a write names: the SHA-256 of the text, in lowercase hex ([`digest_hex`]).
    #[must_use]
    pub fn version(&self) -> RuleSetVersion {
        RuleSetVersion::new(digest_hex(&self.text))
    }

    /// The fingerprint a rule read from this listing carries in its origin: the first eight
    /// bytes of the digest.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let digest = self.digest();
        let mut first = [0; 8];
        first.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(first)
    }

    /// The origin of the rule at `index`: its position in this listing, named by its
    /// [`fingerprint`](Self::fingerprint), so a rule read from one is never taken for an object of
    /// another.
    #[must_use]
    pub fn origin(&self, index: usize) -> RuleOrigin {
        RuleOrigin::Listed {
            set: self.fingerprint(),
            index,
        }
    }

    /// The position of the rule `rule` was read as, where it was read from this listing of `len`
    /// rules.
    #[must_use]
    pub fn index_of(&self, rule: &MailRule, len: usize) -> Option<usize> {
        match rule.origin {
            Some(RuleOrigin::Listed { set, index }) if set == self.fingerprint() && index < len => {
                Some(index)
            }
            _ => None,
        }
    }

    /// The listing `base` was read as: its raw form as `parse` reads it (the adapter's parse,
    /// which keeps the listing in its own order and encoding), with `listing` its text. Refused
    /// ([`unread`](Self::unread)) where `base` names no version or holds no raw form, where the
    /// raw form does not parse, or where the version is not the parsed listing's. A host that
    /// stored the raw form and handed it back re-encoded (other whitespace, other key order) still
    /// names the listing it read.
    ///
    /// # Errors
    ///
    /// [`unread`](Self::unread), as above.
    pub fn of_base<L>(
        base: &RuleSet,
        parse: impl FnOnce(&RawRules) -> Option<L>,
        listing: impl Fn(&L) -> &Self,
    ) -> ProviderResult<L> {
        let version = base.version.as_ref().ok_or_else(Self::unread)?;
        let parsed = base.raw.as_ref().and_then(parse).ok_or_else(Self::unread)?;
        if listing(&parsed).version() != *version {
            return Err(Self::unread());
        }
        Ok(parsed)
    }

    /// Whether this listing, read now, is still the one `base` was read as: a write built on a
    /// listing that moved since is refused as a `Conflict`, which narrows the race with another
    /// client to the time the write takes and does not close it.
    ///
    /// # Errors
    ///
    /// A `Conflict` where the versions differ.
    pub fn unchanged_since(&self, base: &RuleSet) -> ProviderResult<()> {
        if base.version.as_ref() == Some(&self.version()) {
            return Ok(());
        }
        Err(ProviderError::conflict(
            "the mail rules changed since they were read; read them again",
        ))
    }

    /// The refusal of a base not read from the account: `Permanent`, since writing it again
    /// unchanged will never do.
    #[must_use]
    pub fn unread() -> ProviderError {
        ProviderError::new(
            FailureClass::Permanent,
            "a write of the mail rules needs them as they were read, version and raw form; \
             read them first",
        )
    }

    /// The rules handed in whose outcome is [`NotKept`](Kept::NotKept) and that name a rule of
    /// this listing of `len` that `still_there(source, index)` says the write left as it was for
    /// that rule: the edit did not apply, and the rule as read is still there
    /// ([`RulesReceipt::left_as_read`](engine_core::rules::RulesReceipt::left_as_read)). A rule
    /// whose write may have landed (a failure in `failures` with
    /// [`outcome_unknown`](RuleWriteFailure::outcome_unknown): a timeout, a gateway's `502` or
    /// `504`) is left out: its edit may have applied, which only a read can tell.
    #[must_use]
    pub fn left_as_read(
        &self,
        rules: &[MailRule],
        outcomes: &[RuleOutcome],
        failures: &[RuleWriteFailure],
        len: usize,
        still_there: impl Fn(usize, usize) -> bool,
    ) -> Vec<usize> {
        let unknown = |source: usize| {
            failures
                .iter()
                .any(|failed| failed.outcome_unknown && failed.rule == FailedRule::Written(source))
        };
        outcomes
            .iter()
            .filter(|outcome| outcome.kept == Kept::NotKept && !unknown(outcome.source))
            .filter_map(|outcome| {
                let index = self.index_of(rules.get(outcome.source)?, len)?;
                still_there(outcome.source, index).then_some(outcome.source)
            })
            .collect()
    }
}

/// The SHA-256 of `text` in lowercase hex: the version a transport with no version of its own
/// gives a setting as it read it (a rule listing, an out-of-office reply), compared again before
/// a guarded write.
#[must_use]
pub fn digest_hex(text: &str) -> String {
    use core::fmt::Write as _;

    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
#[path = "rule_listing_tests.rs"]
mod tests;
