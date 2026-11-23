//! Normalized mailbox roles.

use core::fmt;

use serde::{Deserialize, Serialize};

/// The normalized role of a mail collection, mapped from provider roles and
/// distinct from the collection's id and display name (`modeling.md`).
///
/// The set is **open**: the IANA mailbox-role registry is extensible, so
/// unrecognized roles are preserved verbatim in [`MailboxRole::Other`] rather
/// than dropped. Spec-backed mappers are provided for the JMAP `role` (an IANA
/// attribute name) and IMAP SPECIAL-USE attributes (RFC 6154/8457); Gmail system
/// labels and Microsoft Graph well-known folder names are mapped by their
/// respective adapters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum MailboxRole {
    /// The primary inbox.
    Inbox,
    /// Archived messages.
    Archive,
    /// Unsent drafts.
    Drafts,
    /// Copies of sent messages.
    Sent,
    /// Deleted messages awaiting purge.
    Trash,
    /// Junk/spam.
    Junk,
    /// A virtual collection of all messages.
    All,
    /// A virtual collection of flagged ("important") messages.
    Flagged,
    /// Messages marked important (RFC 8457 `\Important`).
    Important,
    /// An unrecognized role, preserved verbatim (lowercased).
    Other(String),
}

impl MailboxRole {
    /// Maps a JMAP `role` value (a lowercase IANA attribute name, RFC 8621 §2).
    #[must_use]
    pub fn from_jmap_role(role: &str) -> Self {
        match role.to_ascii_lowercase().as_str() {
            "inbox" => Self::Inbox,
            "archive" => Self::Archive,
            "drafts" => Self::Drafts,
            "sent" => Self::Sent,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            "important" => Self::Important,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Maps an IMAP SPECIAL-USE attribute (RFC 6154/8457), e.g. `\Sent`.
    ///
    /// IMAP `INBOX` is identified by its reserved, case-insensitive *name*, not a
    /// SPECIAL-USE attribute, so it is never produced here; the IMAP adapter maps
    /// the `INBOX` name to [`MailboxRole::Inbox`] separately.
    #[must_use]
    pub fn from_imap_special_use(attribute: &str) -> Self {
        match attribute
            .trim_start_matches('\\')
            .to_ascii_lowercase()
            .as_str()
        {
            "archive" => Self::Archive,
            "drafts" => Self::Drafts,
            "sent" => Self::Sent,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            "important" => Self::Important,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The IMAP `SPECIAL-USE` attribute (RFC 6154, RFC 8457) for this role, backslash included,
    /// which Sieve's `special-use` extension names a folder by too (RFC 8579): the inverse of
    /// [`from_imap_special_use`](Self::from_imap_special_use). `None` for the inbox, which has
    /// none (it is `INBOX` by name), and for a role no attribute names.
    #[must_use]
    pub fn as_imap_special_use(&self) -> Option<&'static str> {
        match self {
            Self::Archive => Some("\\Archive"),
            Self::Drafts => Some("\\Drafts"),
            Self::Sent => Some("\\Sent"),
            Self::Trash => Some("\\Trash"),
            Self::Junk => Some("\\Junk"),
            Self::All => Some("\\All"),
            Self::Flagged => Some("\\Flagged"),
            Self::Important => Some("\\Important"),
            Self::Inbox | Self::Other(_) => None,
        }
    }

    /// Returns the canonical JMAP/IANA role name.
    #[must_use]
    pub fn as_jmap_role(&self) -> &str {
        match self {
            Self::Inbox => "inbox",
            Self::Archive => "archive",
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::All => "all",
            Self::Flagged => "flagged",
            Self::Important => "important",
            Self::Other(role) => role,
        }
    }
}

impl fmt::Display for MailboxRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_jmap_role())
    }
}

impl From<String> for MailboxRole {
    fn from(value: String) -> Self {
        Self::from_jmap_role(&value)
    }
}

impl From<MailboxRole> for String {
    fn from(value: MailboxRole) -> Self {
        value.as_jmap_role().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jmap_roles_map_to_known_variants() {
        assert_eq!(MailboxRole::from_jmap_role("inbox"), MailboxRole::Inbox);
        assert_eq!(MailboxRole::from_jmap_role("JUNK"), MailboxRole::Junk);
        assert_eq!(
            MailboxRole::from_jmap_role("vendorspecific"),
            MailboxRole::Other("vendorspecific".into())
        );
    }

    #[test]
    fn a_special_use_attribute_and_its_role_map_both_ways() {
        for role in [
            MailboxRole::Archive,
            MailboxRole::Drafts,
            MailboxRole::Sent,
            MailboxRole::Trash,
            MailboxRole::Junk,
            MailboxRole::All,
            MailboxRole::Flagged,
            MailboxRole::Important,
        ] {
            let attribute = role.as_imap_special_use().unwrap();
            assert_eq!(MailboxRole::from_imap_special_use(attribute), role);
        }
        assert_eq!(MailboxRole::Inbox.as_imap_special_use(), None);
        assert_eq!(MailboxRole::Other("x".into()).as_imap_special_use(), None);
    }

    #[test]
    fn imap_special_use_maps_consistently_with_jmap() {
        // The same normalized role regardless of which provider's spelling.
        assert_eq!(
            MailboxRole::from_imap_special_use("\\Sent"),
            MailboxRole::from_jmap_role("sent")
        );
        assert_eq!(
            MailboxRole::from_imap_special_use("\\Archive"),
            MailboxRole::Archive
        );
        assert_eq!(
            MailboxRole::from_imap_special_use("\\Flagged"),
            MailboxRole::Flagged
        );
    }

    #[test]
    fn role_roundtrips_through_string_and_json() {
        for role in [
            MailboxRole::Inbox,
            MailboxRole::Sent,
            MailboxRole::Important,
            MailboxRole::Other("custom".into()),
        ] {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(serde_json::from_str::<MailboxRole>(&json).unwrap(), role);
            assert_eq!(role.to_string(), role.as_jmap_role());
        }
        assert_eq!(
            serde_json::to_string(&MailboxRole::Inbox).unwrap(),
            "\"inbox\""
        );
    }
}
