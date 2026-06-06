//! JSON-pointer patch objects.
//!
//! A [`PatchObject`] is the partial-update representation shared by JMAP `/set`
//! updates (RFC 8620 §5.3) and JSCalendar `recurrenceOverrides` and
//! `localizations` (RFC 8984 §1.4.9). Each key is a JSON Pointer (RFC 6901) with
//! an implicit leading `/`; the value replaces the property, or — when `null` —
//! resets it to its default or removes it.
//!
//! The engine validates the one rule it can check without the target object: **no
//! patch path may be a prefix of another**. The remaining rules (a path must not
//! point inside an array, and every parent must already exist) depend on the
//! target and are enforced where the patch is applied, which is the expansion
//! layer, not this crate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error returned when a [`PatchObject`] is structurally invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PatchError {
    /// A patch path was the empty string.
    #[error("a patch path must not be empty")]
    EmptyPath,
    /// One patch path is a prefix of another (e.g. `alerts` and `alerts/1`),
    /// which is forbidden because the two would overlap.
    #[error("patch path {prefix:?} overlaps with {extension:?}")]
    PrefixOverlap {
        /// The shorter path.
        prefix: String,
        /// The path it is a prefix of.
        extension: String,
    },
}

/// A validated set of JSON-pointer patches.
///
/// Does not implement `Eq`/`Hash`: patch values are arbitrary JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BTreeMap<String, Value>", into = "BTreeMap<String, Value>")]
pub struct PatchObject(BTreeMap<String, Value>);

/// Returns `true` if `shorter` is a JSON-pointer prefix of `longer`
/// (`longer` continues with a `/` after `shorter`).
fn is_pointer_prefix(shorter: &str, longer: &str) -> bool {
    longer.len() > shorter.len()
        && longer.starts_with(shorter)
        && longer.as_bytes()[shorter.len()] == b'/'
}

impl PatchObject {
    /// Builds a patch object from path/value pairs, validating the no-overlap
    /// rule.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::EmptyPath`] for an empty path, or
    /// [`PatchError::PrefixOverlap`] if one path is a prefix of another.
    pub fn new(patches: impl IntoIterator<Item = (String, Value)>) -> Result<Self, PatchError> {
        let map: BTreeMap<String, Value> = patches.into_iter().collect();
        for path in map.keys() {
            if path.is_empty() {
                return Err(PatchError::EmptyPath);
            }
        }
        let keys: Vec<&String> = map.keys().collect();
        for (i, outer) in keys.iter().enumerate() {
            for inner in &keys[i + 1..] {
                if is_pointer_prefix(outer, inner) {
                    return Err(PatchError::PrefixOverlap {
                        prefix: (*outer).clone(),
                        extension: (*inner).clone(),
                    });
                }
            }
        }
        Ok(Self(map))
    }

    /// Returns the patch value for a path, if present. A stored `Value::Null`
    /// means "reset to default / remove".
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.0.get(path)
    }

    /// Returns `true` if there are no patches.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of patches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterates over the patch paths and values.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

impl TryFrom<BTreeMap<String, Value>> for PatchObject {
    type Error = PatchError;

    fn try_from(value: BTreeMap<String, Value>) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PatchObject> for BTreeMap<String, Value> {
    fn from(value: PatchObject) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_patch_is_accepted() {
        let patch = PatchObject::new([
            ("title".to_owned(), json!("New title")),
            ("locations/loc1/name".to_owned(), json!("Room A")),
            ("description".to_owned(), Value::Null),
        ])
        .unwrap();
        assert_eq!(patch.len(), 3);
        assert_eq!(patch.get("description"), Some(&Value::Null));
    }

    #[test]
    fn prefix_overlap_is_rejected() {
        let err = PatchObject::new([
            ("alerts".to_owned(), json!({})),
            ("alerts/1/offset".to_owned(), json!("-PT5M")),
        ])
        .unwrap_err();
        assert!(matches!(err, PatchError::PrefixOverlap { .. }));
    }

    #[test]
    fn sibling_paths_do_not_overlap() {
        // `alerts/1` and `alerts/2` share a parent but neither is a prefix of
        // the other.
        let patch = PatchObject::new([
            ("alerts/1/offset".to_owned(), json!("-PT5M")),
            ("alerts/2/offset".to_owned(), json!("-PT10M")),
        ]);
        assert!(patch.is_ok());
    }

    #[test]
    fn empty_path_is_rejected() {
        assert_eq!(
            PatchObject::new([(String::new(), json!(1))]),
            Err(PatchError::EmptyPath)
        );
    }

    #[test]
    fn roundtrips_through_json() {
        let patch = PatchObject::new([("title".to_owned(), json!("x"))]).unwrap();
        let encoded = serde_json::to_string(&patch).unwrap();
        assert_eq!(encoded, r#"{"title":"x"}"#);
        let back: PatchObject = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, patch);
        // Overlap is rejected on deserialize too.
        assert!(serde_json::from_str::<PatchObject>(r#"{"a":1,"a/b":2}"#).is_err());
    }
}
