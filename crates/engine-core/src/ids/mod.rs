//! Identity newtypes.
//!
//! Identity in this engine is always the triple `(AccountId, type, provider
//! key)`. One engine instance hosts multiple accounts, so [`AccountId`] scopes
//! every object, sync scope, cursor, and write; a provider key is only unique
//! within an `(account, type)` pair (RFC 8620 §1.6.3), never globally.
//!
//! Two families of id live here:
//!
//! - **Object keys** wrap a [`ProviderKey`]: opaque, provider-assigned,
//!   immutable, and stable across container moves (the adapter substitutes a
//!   provider's immutable-id form where its natural id is not — Graph default
//!   ids change on move). [`AccountId`], [`MailboxId`], [`MessageId`],
//!   [`ThreadId`], [`BlobId`], [`PartId`], [`CalendarId`], and [`EventId`] are
//!   distinct types so a mailbox key can never be passed where a message key is
//!   expected.
//! - **Content ids** are carried *inside* a message or event and identify it
//!   across systems rather than within one provider: [`MessageIdHeader`] (the
//!   RFC 5322 `Message-ID`, a threading/reconciliation hint, not hard identity)
//!   and [`Uid`] (the iCalendar/JSCalendar `UID`, the scheduling reconciliation
//!   key).
//!
//! The generic [`ProviderKey`] is intentionally **not** constrained to the JMAP
//! `Id` alphabet: it must also hold Microsoft Graph, Gmail, and CalDAV keys,
//! which use different character sets. Alphabet validation (e.g. JMAP's
//! URL-safe-base64 subset) belongs in the relevant provider adapter, not here.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Error returned when constructing an identity or [`ProviderKey`] fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    /// The supplied identity string was empty.
    #[error("identity must not be empty")]
    Empty,
    /// The supplied identity string exceeded the maximum length for its kind.
    #[error("identity is too long: {actual} octets exceeds the limit of {limit}")]
    TooLong {
        /// Maximum number of octets allowed for this id kind.
        limit: usize,
        /// Actual number of octets supplied.
        actual: usize,
    },
}

/// An opaque, provider-assigned object key.
///
/// A provider key is the immutable handle a provider uses for one object. It is
/// treated as opaque bytes: the engine never parses it, only stores and compares
/// it. The only universal invariant across providers is that it is non-empty;
/// length and alphabet limits are provider-specific and enforced by adapters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderKey(Box<str>);

impl ProviderKey {
    /// Creates a provider key from any string-like value.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::Empty`] if `value` is empty.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty);
        }
        Ok(Self(value.into_boxed_str()))
    }

    /// Returns the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProviderKey {
    type Error = IdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProviderKey> for String {
    fn from(key: ProviderKey) -> Self {
        key.0.into()
    }
}

/// Defines a distinct object-key newtype wrapping a [`ProviderKey`].
///
/// The generated type is a transparent newtype: it serializes as the underlying
/// provider-key string, validating on the way in. Distinct types mean the
/// compiler rejects passing, say, a [`MailboxId`] where a [`MessageId`] is
/// expected.
macro_rules! object_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(ProviderKey);

        impl $name {
            #[doc = concat!("Wraps a validated [`ProviderKey`] as a [`", stringify!($name), "`].")]
            #[must_use]
            pub fn new(key: ProviderKey) -> Self {
                Self(key)
            }

            #[doc = "Returns the underlying provider key."]
            #[must_use]
            pub fn key(&self) -> &ProviderKey {
                &self.0
            }

            #[doc = "Returns the id as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl ::core::convert::TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> ::core::result::Result<Self, Self::Error> {
                Ok(Self(ProviderKey::new(value)?))
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

/// Defines a length-bounded, non-empty string newtype for a *content* id (one
/// carried inside a message or event, identifying it across systems).
macro_rules! content_id {
    ($(#[$meta:meta])* $name:ident, max_octets = $limit:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Box<str>);

        impl $name {
            #[doc = "Maximum length, in octets, accepted for this id."]
            pub const MAX_OCTETS: usize = $limit;

            #[doc = concat!("Creates a [`", stringify!($name), "`] from a string-like value.")]
            ///
            /// # Errors
            ///
            /// Returns [`IdError::Empty`] if empty, or [`IdError::TooLong`] if it
            /// exceeds [`Self::MAX_OCTETS`].
            pub fn new(value: impl Into<String>) -> ::core::result::Result<Self, IdError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(IdError::Empty);
                }
                if value.len() > $limit {
                    return Err(IdError::TooLong { limit: $limit, actual: value.len() });
                }
                Ok(Self(value.into_boxed_str()))
            }

            #[doc = "Returns the id as a string slice."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl ::core::convert::TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> ::core::result::Result<Self, Self::Error> {
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

// The generator macros above are visible to the submodules below through
// textual macro scoping (they are declared before the `mod` statements).
mod account;
mod calendar;
mod dav;
mod mail;

pub use account::AccountId;
pub use calendar::{CalendarId, EventId, Uid};
pub use dav::DavCollectionId;
pub use mail::{BlobId, MailboxId, MessageId, MessageIdHeader, PartId, ThreadId};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_key_rejects_empty() {
        assert_eq!(ProviderKey::new(""), Err(IdError::Empty));
    }

    #[test]
    fn provider_key_preserves_arbitrary_alphabets() {
        // Graph ids contain `=`/`/`, CalDAV hrefs contain `/` and `.ics`,
        // Gmail uses base32hex. None of these are valid JMAP `Id`s, but the
        // generic provider key must accept all of them verbatim.
        for raw in ["AAMkAD==/", "calendars/work/abc.ics", "18c0f1a2b3c4d5e6"] {
            let key = ProviderKey::new(raw).expect("non-empty key");
            assert_eq!(key.as_str(), raw);
        }
    }

    #[test]
    fn provider_key_is_case_sensitive() {
        // RFC 8620 §1.2: ids are case-sensitive; `A` != `a`.
        assert_ne!(
            ProviderKey::new("Abc").unwrap(),
            ProviderKey::new("abc").unwrap()
        );
    }

    #[test]
    fn object_ids_share_a_runtime_surface_but_not_a_type() {
        let key = ProviderKey::new("m1").unwrap();
        let mailbox = MailboxId::new(key.clone());
        let message = MessageId::new(key);
        assert_eq!(mailbox.as_str(), message.as_str());
        // `let _: MailboxId = message;` would not compile — the point of the
        // distinct newtypes.
    }

    #[test]
    fn object_id_try_from_str_validates() {
        assert_eq!(MailboxId::try_from(""), Err(IdError::Empty));
        assert_eq!(MailboxId::try_from("inbox").unwrap().as_str(), "inbox");
    }

    #[test]
    fn object_id_roundtrips_through_json() {
        let id = EventId::try_from("evt-42").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"evt-42\"");
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn object_id_deserialization_rejects_empty() {
        let err = serde_json::from_str::<MessageId>("\"\"").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn display_matches_inner_string() {
        let id = ThreadId::try_from("t-1").unwrap();
        assert_eq!(id.to_string(), "t-1");
        assert_eq!(ProviderKey::new("pk").unwrap().to_string(), "pk");
    }
}
