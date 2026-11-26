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
///
/// The enum is deliberately not `#[non_exhaustive]`: a host matches it exhaustively, so a new
/// role breaks its build until the host decides where that role goes.
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
    /// A folder the mail service keeps for its own bookkeeping, holding no mail the user filed:
    /// the logs of failed synchronisations and the copies a sync conflict left behind
    /// (Exchange's Sync Issues and the folders under it). Its name is localized like any
    /// folder's, so the role is what identifies it.
    ///
    /// No registry names it: it has no IMAP SPECIAL-USE attribute and no IANA JMAP role, and
    /// its name, `server-records`, is the engine's own. Microsoft Graph names each such folder
    /// by a well-known name (`graph.md`, "Roles resolved by id").
    ServerRecords,
    /// A folder of chat transcripts the service stored as messages: Exchange's Conversation
    /// History, which mainly holds Skype for Business conversations (Microsoft 365 keeps its
    /// copies of Teams chats in a hidden folder under it, which a folder listing does not
    /// return). They are records of chats, not mail anyone sent or the user filed. Its name is
    /// localized like any folder's, so the role is what identifies it.
    ///
    /// Gmail's chats (its `CHAT` label) are the same thing, but no Gmail folder carries this
    /// role today: the Gmail adapter treats `CHAT` as a label that files nothing, so a chat
    /// appears only in the synthetic All Mail ([`All`](Self::All)) and nothing marks it as a
    /// chat. Leaving this role out does not leave Gmail's chats out.
    ///
    /// No registry names it either: no IMAP SPECIAL-USE attribute and no IANA JMAP role; its
    /// name, `chat-history`, is the engine's own.
    ChatHistory,
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
            // No JMAP server reports these names; they are here so each role reads back as itself.
            "server-records" => Self::ServerRecords,
            "chat-history" => Self::ChatHistory,
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
            Self::Inbox | Self::ServerRecords | Self::ChatHistory | Self::Other(_) => None,
        }
    }

    /// The role's name: the JMAP role (a lowercase IANA attribute name) of a registered role, the
    /// server's own spelling of an [`Other`](Self::Other), and the engine's own name for
    /// [`ServerRecords`](Self::ServerRecords) and [`ChatHistory`](Self::ChatHistory), which no
    /// registry names and so are no role to send a JMAP server. It is also the role's
    /// [`Display`](fmt::Display) and serde form, which [`from_jmap_role`](Self::from_jmap_role)
    /// reads back.
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
            Self::ServerRecords => "server-records",
            Self::ChatHistory => "chat-history",
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

    #[test]
    fn the_roles_no_registry_names_have_names_of_their_own_and_no_special_use() {
        for (role, name) in [
            (MailboxRole::ServerRecords, "server-records"),
            (MailboxRole::ChatHistory, "chat-history"),
        ] {
            // No registry names them, so no IMAP attribute writes one or reads as one.
            assert_eq!(role.as_imap_special_use(), None, "{name}");
            assert_eq!(
                MailboxRole::from_imap_special_use(&format!("\\{name}")),
                MailboxRole::Other(name.into())
            );
            assert_eq!(role.as_jmap_role(), name);
            assert_eq!(role.to_string(), name);
            // Each survives a store's round trip by its name, like every other role.
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<MailboxRole>(&json).unwrap(), role);
        }
        // A name no role has still reads as one the engine does not know.
        assert_eq!(
            serde_json::from_str::<MailboxRole>("\"snoozed\"").unwrap(),
            MailboxRole::Other("snoozed".into())
        );
    }
}
