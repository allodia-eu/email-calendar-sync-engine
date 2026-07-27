//! Conservative email canonicalization.

use core::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// A valid email reduced to the engine's conservative equality key.
///
/// Only the domain is IDNA-normalized and case-folded. The local part remains
/// exact, so `Case@example.test` and `case@example.test` are distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CanonicalEmail(Box<str>);

/// Why an email could not become a conservative identity key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CanonicalEmailError {
    /// The value does not have one non-empty local part and domain.
    #[error("email must contain a non-empty local part and domain")]
    InvalidShape,
    /// The internationalized domain is invalid.
    #[error("invalid internationalized email domain: {0}")]
    InvalidDomain(String),
}

impl CanonicalEmail {
    /// Parses and canonicalizes an address.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalEmailError`] for a missing local/domain part or an
    /// invalid IDNA domain.
    pub fn parse(value: &str) -> Result<Self, CanonicalEmailError> {
        let value = value.trim();
        let (local, domain) = value
            .rsplit_once('@')
            .ok_or(CanonicalEmailError::InvalidShape)?;
        if local.is_empty() || domain.is_empty() || local.contains('@') {
            return Err(CanonicalEmailError::InvalidShape);
        }
        let domain = idna::domain_to_ascii(domain)
            .map_err(|error| CanonicalEmailError::InvalidDomain(error.to_string()))?
            .to_lowercase();
        if domain.is_empty() {
            return Err(CanonicalEmailError::InvalidShape);
        }
        Ok(Self(format!("{local}@{domain}").into_boxed_str()))
    }

    /// Returns the canonical address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalEmail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CanonicalEmail {
    type Err = CanonicalEmailError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for CanonicalEmail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}
