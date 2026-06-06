//! Provider extended properties.
//!
//! Providers attach custom data that is neither a first-class field nor a raw
//! payload: Microsoft Graph single/multi-value extended properties and open
//! extensions, Google Calendar `extendedProperties` (private/shared), Gmail
//! classification labels. The engine preserves these as **normalized, namespaced
//! key-value data** so they survive sync and can be re-derived without loss
//! (`modeling.md`).
//!
//! Keys are namespaced strings by convention (for example
//! `"microsoft.graph/<guid>"` or `"google/private/<name>"`); the namespacing
//! scheme is owned by the adapter that produced them. The engine stores them
//! verbatim and never interprets them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A namespaced bag of provider-defined extended properties.
///
/// This type intentionally does not implement `Eq`/`Hash`: values are arbitrary
/// JSON (which may contain floats), so only structural [`PartialEq`] is offered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtendedProperties(BTreeMap<String, Value>);

impl ExtendedProperties {
    /// Returns an empty set of extended properties.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the value stored under a namespaced key, if any.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Sets a namespaced key to a value, returning the previous value if the key
    /// was already present.
    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(key.into(), value)
    }

    /// Returns `true` if no extended properties are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of stored properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterates over the namespaced keys and their values in sorted key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_get_and_iterate() {
        let mut props = ExtendedProperties::new();
        assert!(props.is_empty());
        assert!(
            props
                .set("google/private/color", json!("#ff0000"))
                .is_none()
        );
        assert_eq!(
            props.set("google/private/color", json!("#00ff00")),
            Some(json!("#ff0000"))
        );
        assert_eq!(props.get("google/private/color"), Some(&json!("#00ff00")));
        assert_eq!(props.len(), 1);
        assert_eq!(props.iter().count(), 1);
    }

    #[test]
    fn roundtrips_losslessly_through_json() {
        let mut props = ExtendedProperties::new();
        props.set("microsoft.graph/x", json!({"nested": [1, 2, 3]}));
        let encoded = serde_json::to_string(&props).unwrap();
        let back: ExtendedProperties = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, props);
    }
}
