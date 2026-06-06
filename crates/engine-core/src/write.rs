//! The write (outbox) contract.
//!
//! Every UI-visible write is a durable [`PendingOp`] before any provider side
//! effect (`north-star.md` Write Contract). The pure data shapes live here; the
//! async outbox worker and the store's fenced claim/resolve live in
//! `engine-store`/`engine-sync`.
//!
//! Invariants this contract encodes:
//!
//! - **Idempotent enqueue** — each op carries an [`IdempotencyKey`]; re-enqueuing
//!   the same key returns the existing [`PendingOpId`] rather than duplicating.
//! - **Dependencies** — an op may `depends_on` earlier ops, so offline
//!   create-then-edit flows order correctly; a [`CreationId`] stands in for a
//!   provider id that is not yet known.
//! - **Serialized resources** — ops sharing a [`ResourceKey`] are not run
//!   concurrently.
//! - **Ambiguous sends never blind-retry** — an ambiguous outcome enters
//!   [`PendingOutcome::NeedsConfirmation`], distinct from a plain failure.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::FailureClass;
use crate::ids::ProviderKey;
use crate::time::Duration;

/// Defines a non-empty string newtype used by the write contract.
macro_rules! nonempty_str {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Box<str>);

        impl $name {
            #[doc = concat!("Creates a [`", stringify!($name), "`].")]
            ///
            /// # Errors
            ///
            /// Returns [`WriteKeyError`] if the value is empty.
            pub fn new(value: impl Into<String>) -> Result<Self, WriteKeyError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(WriteKeyError);
                }
                Ok(Self(value.into_boxed_str()))
            }

            #[doc = "Returns the value as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::core::convert::TryFrom<String> for $name {
            type Error = WriteKeyError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl ::core::convert::From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.into()
            }
        }
    };
}

/// Error returned when a write-contract key is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("write key must not be empty")]
pub struct WriteKeyError;

nonempty_str! {
    /// A client-supplied key that makes enqueuing idempotent: re-enqueuing the
    /// same key returns the existing [`PendingOpId`].
    IdempotencyKey
}

nonempty_str! {
    /// Identifies the provider resource an op targets; ops sharing one are
    /// serialized so writes to the same resource never race.
    ResourceKey
}

nonempty_str! {
    /// A client-minted local id standing in for a provider id that is not yet
    /// known, used to wire up offline create-then-edit dependency chains until
    /// the create resolves (JMAP creation-id concept, RFC 8620 §5.3).
    CreationId
}

/// A store-assigned id for a durable pending operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingOpId(u64);

impl PendingOpId {
    /// Wraps a store-assigned id value.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the raw id value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A durable pending write operation.
///
/// The `payload` shape (create draft, set keywords, move, submit, RSVP, …) is
/// defined by the outbox/provider layer; this contract fixes the envelope that
/// the store reasons about. Does not implement `Eq`: payloads are arbitrary JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    /// Makes enqueuing idempotent.
    pub idempotency_key: IdempotencyKey,
    /// Earlier ops that must reach terminal success before this one runs.
    pub depends_on: Vec<PendingOpId>,
    /// The resource this op serializes on.
    pub resource_key: ResourceKey,
    /// The operation description, interpreted by the outbox/provider layer.
    pub payload: Value,
}

impl PendingOp {
    /// Creates a pending op with no dependencies.
    #[must_use]
    pub fn new(idempotency_key: IdempotencyKey, resource_key: ResourceKey, payload: Value) -> Self {
        Self {
            idempotency_key,
            depends_on: Vec::new(),
            resource_key,
            payload,
        }
    }
}

/// The result of attempting a pending operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingOutcome {
    /// The op succeeded and the provider assigned this key (resolving any
    /// creation id).
    Succeeded {
        /// The provider key now backing the created/updated object.
        provider_key: ProviderKey,
    },
    /// The op failed; classified for retry/resync decisions.
    Failed {
        /// Why it failed.
        class: FailureClass,
        /// How long to wait before retrying, if retryable.
        retry_after: Option<Duration>,
    },
    /// The outcome is genuinely ambiguous (e.g. a post-DATA SMTP failure that may
    /// or may not have delivered). The op must **not** be blindly retried; it is
    /// resolved by sync reconciliation, Message-ID lookup, or explicit
    /// confirmation.
    NeedsConfirmation {
        /// Human-facing context for the ambiguity.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_reject_empty() {
        assert_eq!(IdempotencyKey::new(""), Err(WriteKeyError));
        assert_eq!(ResourceKey::new(""), Err(WriteKeyError));
        assert_eq!(CreationId::new(""), Err(WriteKeyError));
        assert_eq!(IdempotencyKey::new("k1").unwrap().as_str(), "k1");
    }

    #[test]
    fn pending_op_defaults_to_no_dependencies() {
        let op = PendingOp::new(
            IdempotencyKey::new("idem-1").unwrap(),
            ResourceKey::new("message:m1").unwrap(),
            json!({ "op": "setKeywords", "add": ["$seen"] }),
        );
        assert!(op.depends_on.is_empty());
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(serde_json::from_str::<PendingOp>(&json).unwrap(), op);
    }

    #[test]
    fn dependencies_order_offline_chains() {
        let create = PendingOpId::new(1);
        let mut edit = PendingOp::new(
            IdempotencyKey::new("idem-2").unwrap(),
            ResourceKey::new("draft:#local-1").unwrap(),
            json!({ "op": "update" }),
        );
        edit.depends_on.push(create);
        assert_eq!(edit.depends_on, vec![create]);
    }

    #[test]
    fn outcomes_distinguish_failure_from_ambiguity() {
        let succeeded = PendingOutcome::Succeeded {
            provider_key: ProviderKey::new("server-id").unwrap(),
        };
        let failed = PendingOutcome::Failed {
            class: FailureClass::RateLimited,
            retry_after: Some("PT30S".parse().unwrap()),
        };
        let ambiguous = PendingOutcome::NeedsConfirmation {
            detail: "post-DATA timeout".into(),
        };
        for outcome in [&succeeded, &failed, &ambiguous] {
            let json = serde_json::to_string(outcome).unwrap();
            assert_eq!(
                &serde_json::from_str::<PendingOutcome>(&json).unwrap(),
                outcome
            );
        }
        assert_ne!(failed, ambiguous);
    }
}
