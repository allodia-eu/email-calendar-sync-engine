//! JSContact property-map primitives.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{extended::ExtendedProperties, ids::IdError};

/// A JSContact property-map key.
///
/// Property ids are preserved because other properties can refer to them (for
/// example a title's `organizationId`) and because rewriting them would make a
/// raw-preserving patch needlessly destructive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PropertyId(Box<str>);

impl PropertyId {
    /// Creates a non-empty property id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::Empty`] for an empty id.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            Err(IdError::Empty)
        } else {
            Ok(Self(value.into_boxed_str()))
        }
    }

    /// Returns the property id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PropertyId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One entry in a JSContact-style property map.
///
/// `contexts` stays open because JSContact extensions may define new context
/// names. Unknown per-property values belong in namespaced `extensions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactProperty<T> {
    /// The typed normalized value.
    pub value: T,
    /// Context names such as `work` or `private`.
    pub contexts: BTreeSet<String>,
    /// Preference rank; lower positive values are preferred.
    pub preference: Option<u8>,
    /// A provider/user supplied label.
    pub label: Option<String>,
    /// Unknown or provider-specific property data.
    pub extensions: ExtendedProperties,
}

impl<T> ContactProperty<T> {
    /// Wraps a value with no metadata.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            value,
            contexts: BTreeSet::new(),
            preference: None,
            label: None,
            extensions: ExtendedProperties::new(),
        }
    }

    /// Wraps a preferred value.
    #[must_use]
    pub fn preferred(value: T, preference: u8) -> Self {
        Self {
            preference: Some(preference),
            ..Self::new(value)
        }
    }
}

impl<T: Default> Default for ContactProperty<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
