//! Time-zone identifiers.

use core::fmt;

use serde::{Deserialize, Serialize};

use super::TimeError;

/// A time-zone identifier, recording which source resolves it to UTC offsets.
///
/// Per `calendar-semantics.md`, embedded `VTIMEZONE` definitions may disagree
/// with the IANA database for the same `TZID`, so the engine records *which*
/// source applies to each value:
///
/// - [`TimeZoneId::Iana`] — a name from the bundled IANA tzdata; the engine
///   expands recurrence with IANA rules (consistent and updatable). The embedded
///   `VTIMEZONE`, if any, is still preserved in `RawIcal`.
/// - [`TimeZoneId::Custom`] — an unknown or custom zone; the engine expands using
///   the embedded `VTIMEZONE` rules carried alongside the event.
///
/// The id string is stored verbatim. This type does **not** validate that an
/// IANA name actually resolves (that needs the tzdata, which lives in another
/// crate); the adapter chooses the variant. Adapters that receive non-IANA zones
/// (e.g. Microsoft Graph's Windows zone names) map them to IANA at their boundary
/// before constructing [`TimeZoneId::Iana`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TimeZoneId {
    /// A zone identified by an IANA tzdata name, e.g. `Europe/Amsterdam`.
    Iana(Box<str>),
    /// A custom zone defined by an embedded `VTIMEZONE`, expanded from its own
    /// rules.
    Custom(Box<str>),
}

impl TimeZoneId {
    /// Creates an IANA-named zone.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::Empty`] if `name` is empty.
    pub fn iana(name: impl Into<String>) -> Result<Self, TimeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(TimeError::Empty);
        }
        Ok(Self::Iana(name.into_boxed_str()))
    }

    /// Creates a custom zone defined by an embedded `VTIMEZONE`.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::Empty`] if `id` is empty.
    pub fn custom(id: impl Into<String>) -> Result<Self, TimeError> {
        let id = id.into();
        if id.is_empty() {
            return Err(TimeError::Empty);
        }
        Ok(Self::Custom(id.into_boxed_str()))
    }

    /// Returns the UTC zone, `Etc/UTC`.
    #[must_use]
    pub fn utc() -> Self {
        Self::Iana("Etc/UTC".into())
    }

    /// Returns the zone identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Iana(s) | Self::Custom(s) => s,
        }
    }

    /// Returns `true` if this is an IANA-named zone (expanded with bundled
    /// tzdata) rather than a custom embedded one.
    #[must_use]
    pub fn is_iana(&self) -> bool {
        matches!(self, Self::Iana(_))
    }
}

impl fmt::Display for TimeZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iana_and_custom_are_distinct_even_with_equal_strings() {
        let iana = TimeZoneId::iana("Made/Up").unwrap();
        let custom = TimeZoneId::custom("Made/Up").unwrap();
        assert_eq!(iana.as_str(), custom.as_str());
        assert_ne!(iana, custom);
        assert!(iana.is_iana());
        assert!(!custom.is_iana());
    }

    #[test]
    fn utc_is_an_iana_zone() {
        assert_eq!(TimeZoneId::utc(), TimeZoneId::iana("Etc/UTC").unwrap());
        assert!(TimeZoneId::utc().is_iana());
    }

    #[test]
    fn empty_zone_is_rejected() {
        assert_eq!(TimeZoneId::iana(""), Err(TimeError::Empty));
        assert_eq!(TimeZoneId::custom(""), Err(TimeError::Empty));
    }

    #[test]
    fn roundtrips_through_json() {
        let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
        let json = serde_json::to_string(&zone).unwrap();
        assert_eq!(serde_json::from_str::<TimeZoneId>(&json).unwrap(), zone);
        assert_eq!(zone.to_string(), "Europe/Amsterdam");
    }
}
