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
    rules::{Kept, MailRule, RuleOrigin, RuleOutcome, RuleSet},
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

    /// The raw form of `base`, where `base` was read from the account: it names a version, holds a
    /// raw form, and the version is the raw form's.
    ///
    /// # Errors
    ///
    /// [`unread`](Self::unread) otherwise.
    pub fn base_raw(base: &RuleSet) -> ProviderResult<&RawRules> {
        let version = base.version.as_ref().ok_or_else(Self::unread)?;
        let raw = base.raw.as_ref().ok_or_else(Self::unread)?;
        if Self::new(raw.as_str()).version() != *version {
            return Err(Self::unread());
        }
        Ok(raw)
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
    /// ([`RulesReceipt::left_as_read`](engine_core::rules::RulesReceipt::left_as_read)).
    #[must_use]
    pub fn left_as_read(
        &self,
        rules: &[MailRule],
        outcomes: &[RuleOutcome],
        len: usize,
        still_there: impl Fn(usize, usize) -> bool,
    ) -> Vec<usize> {
        outcomes
            .iter()
            .filter(|outcome| outcome.kept == Kept::NotKept)
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
