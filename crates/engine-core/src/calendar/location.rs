//! Event locations.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::time::TimeZoneId;

/// Whether a value is relative to an event's start or end (JSCalendar
/// `relativeTo`, RFC 8984 §4.2.5/§4.5.2). Shared by locations and alert
/// triggers. An unrecognized value is treated as omitted by the parsing
/// adapter, so the engine only stores these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelativeTo {
    /// Relative to the event's start.
    Start,
    /// Relative to the event's end.
    End,
}

/// A physical location (JSCalendar `Location`, RFC 8984 §4.2.5).
///
/// A well-formed location has at least one property other than `relative_to`.
/// The `time_zone` supports the "different zone at the end" case (e.g. a flight
/// that lands in another zone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// The location name.
    pub name: Option<String>,
    /// A free-text description.
    pub description: Option<String>,
    /// Location type tags (RFC 4589 registry), preserving unknown values.
    pub location_types: BTreeSet<String>,
    /// Whether this location is the event's start or end location.
    pub relative_to: Option<RelativeTo>,
    /// A geographic `geo:` URI, if known.
    pub coordinates: Option<String>,
    /// A zone associated with this location (e.g. the arrival zone).
    pub time_zone: Option<TimeZoneId>,
}

impl Location {
    /// Creates a named location.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            description: None,
            location_types: BTreeSet::new(),
            relative_to: None,
            coordinates: None,
            time_zone: None,
        }
    }
}

/// A virtual location such as a video call (JSCalendar `VirtualLocation`,
/// RFC 8984 §4.2.6). The `uri` is mandatory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualLocation {
    /// The display name.
    pub name: Option<String>,
    /// A free-text description.
    pub description: Option<String>,
    /// The join URI (mandatory).
    pub uri: String,
    /// Supported features (`audio`, `video`, `chat`, …), preserving unknown
    /// values.
    pub features: BTreeSet<String>,
}

impl VirtualLocation {
    /// Creates a virtual location for the given join URI.
    #[must_use]
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            name: None,
            description: None,
            uri: uri.into(),
            features: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_with_end_zone_roundtrips() {
        let mut loc = Location::named("Gate 22");
        loc.relative_to = Some(RelativeTo::End);
        loc.time_zone = Some(TimeZoneId::iana("America/New_York").unwrap());
        let json = serde_json::to_string(&loc).unwrap();
        assert_eq!(serde_json::from_str::<Location>(&json).unwrap(), loc);
    }

    #[test]
    fn virtual_location_keeps_uri_and_features() {
        let mut vloc = VirtualLocation::new("https://meet.example/abc");
        vloc.features.insert("video".into());
        vloc.features.insert("audio".into());
        assert_eq!(vloc.uri, "https://meet.example/abc");
        let json = serde_json::to_string(&vloc).unwrap();
        assert_eq!(
            serde_json::from_str::<VirtualLocation>(&json).unwrap(),
            vloc
        );
    }

    #[test]
    fn relative_to_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&RelativeTo::Start).unwrap(),
            "\"start\""
        );
        assert_eq!(serde_json::to_string(&RelativeTo::End).unwrap(), "\"end\"");
    }
}
