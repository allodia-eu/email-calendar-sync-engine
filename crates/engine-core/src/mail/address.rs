//! Email addresses.

use serde::{Deserialize, Serialize};

/// A single mailbox address with an optional display name (JMAP `EmailAddress`,
/// RFC 8621 §4.1.2.3).
///
/// The `email` field is the parsed address. Parsing is best-effort: per RFC 8621
/// it MAY be malformed (it is not guaranteed to contain an `@`), so this type
/// does **not** validate it. The original header bytes are recoverable from the
/// preserved raw message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmailAddress {
    /// The display name, if the header supplied one.
    pub name: Option<String>,
    /// The address itself (e.g. `someone@example.com`), best-effort.
    pub email: String,
}

impl EmailAddress {
    /// Creates an address with no display name.
    #[must_use]
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    /// Creates an address with a display name.
    #[must_use]
    pub fn named(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }
}

/// A named group of addresses (JMAP `EmailAddressGroup`, RFC 8621 §4.1.2.4;
/// RFC 5322 address groups).
///
/// Groups are uncommon; adapters that do not preserve group structure flatten to
/// a list of [`EmailAddress`] instead. When `name` is `null`, the addresses were
/// not part of a named group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmailAddressGroup {
    /// The group's display name, or `None` if these addresses are ungrouped.
    pub name: Option<String>,
    /// The addresses in the group.
    pub addresses: Vec<EmailAddress>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_with_and_without_names() {
        let bare = EmailAddress::new("a@example.com");
        assert!(bare.name.is_none());
        let named = EmailAddress::named("Alice", "a@example.com");
        assert_eq!(named.name.as_deref(), Some("Alice"));
        assert_ne!(bare, named);
    }

    #[test]
    fn malformed_address_is_accepted_verbatim() {
        // RFC 8621 §4.1.2.3: the parsed email may be malformed.
        let weird = EmailAddress::new("not-an-address");
        assert_eq!(weird.email, "not-an-address");
    }

    #[test]
    fn group_roundtrips_through_json() {
        let group = EmailAddressGroup {
            name: Some("Team".into()),
            addresses: vec![
                EmailAddress::new("a@example.com"),
                EmailAddress::named("Bob", "b@example.com"),
            ],
        };
        let json = serde_json::to_string(&group).unwrap();
        assert_eq!(
            serde_json::from_str::<EmailAddressGroup>(&json).unwrap(),
            group
        );
    }
}
