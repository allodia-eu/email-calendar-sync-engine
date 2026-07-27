//! Contact-domain identities.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// A provider-assigned address-book identity.
    AddressBookId
}

object_id! {
    /// A provider-assigned contact-card identity.
    ContactId
}

/// A persistent store-local identity for one derived unified person.
///
/// This id never crosses a provider boundary. Zero is reserved so accidental
/// default values cannot become durable identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PersonId(u64);

impl PersonId {
    /// Creates a non-zero person id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::Empty`] for zero, which is the reserved sentinel.
    pub fn new(value: u64) -> Result<Self, IdError> {
        if value == 0 {
            Err(IdError::Empty)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the store-local integer.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for PersonId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}
