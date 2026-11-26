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
    /// Maps a JMAP `role` value a server reported (a lowercase IANA attribute name, RFC 8621
    /// §2). Only the registered roles map to their variants; anything else, the engine's own
    /// names ([`ServerRecords`](Self::ServerRecords), [`ChatHistory`](Self::ChatHistory))
    /// included, is a role this server made up, kept as [`Other`](Self::Other). A server's
    /// `server-records` is not the engine's.
    #[must_use]
    pub fn from_jmap_role(role: &str) -> Self {
        let role = role.to_ascii_lowercase();
        Self::registered(&role).unwrap_or(Self::Other(role))
    }

    /// The role a lowercase IANA name registers, the JMAP `inbox` included.
    fn registered(name: &str) -> Option<Self> {
        Some(match name {
            "inbox" => Self::Inbox,
            "archive" => Self::Archive,
            "drafts" => Self::Drafts,
            "sent" => Self::Sent,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            "important" => Self::Important,
            _ => return None,
        })
    }

    /// Reads back the engine's stable name ([`as_str`](Self::as_str)), ignoring case: every
    /// role's, the engine's own names included. What a store wrote, never what a server sent.
    fn from_stable_name(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "server-records" => Self::ServerRecords,
            "chat-history" => Self::ChatHistory,
            other => Self::registered(other).unwrap_or(Self::Other(name)),
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

    /// The role's stable name in the engine: the lowercase IANA name of a registered role, the
    /// engine's own name for [`ServerRecords`](Self::ServerRecords) (`server-records`) and
    /// [`ChatHistory`](Self::ChatHistory) (`chat-history`), and the server's own spelling of an
    /// [`Other`](Self::Other).
    ///
    /// It is the role's [`Display`](fmt::Display) and serde form, and what a store keeps; it
    /// reads back through `From<String>`. It is **not**
    /// what to send a server: that is [`jmap_role`](Self::jmap_role) or
    /// [`as_imap_special_use`](Self::as_imap_special_use), which have no name for a role no
    /// registry names.
    ///
    /// The round trip is by name, so an [`Other`](Self::Other) whose name is one of the engine's
    /// reads back as that role: a server reporting `server-records` (an `Other`, since
    /// [`from_jmap_role`](Self::from_jmap_role) does not take the engine's names from a server)
    /// is [`ServerRecords`](Self::ServerRecords) after a store's round trip, as an IMAP `\Inbox`
    /// attribute (`Other("inbox")`) reads back as [`Inbox`](Self::Inbox). Both name what the
    /// engine means by them, so nothing a host relies on changes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::ServerRecords => "server-records",
            Self::ChatHistory => "chat-history",
            Self::Other(role) => role,
            Self::Inbox => "inbox",
            Self::Archive => "archive",
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::All => "all",
            Self::Flagged => "flagged",
            Self::Important => "important",
        }
    }

    /// The JMAP `role` to send a server for this role (RFC 8621 §2: a lowercase name from the
    /// IANA "IMAP Mailbox Name Attributes" registry, or `inbox`): the inverse of
    /// [`from_jmap_role`](Self::from_jmap_role).
    ///
    /// `None` for a role no registry names. [`ServerRecords`](Self::ServerRecords) and
    /// [`ChatHistory`](Self::ChatHistory) are the engine's own. [`Other`](Self::Other) is
    /// `None` too: it holds whatever a server reported that the engine does not know (a
    /// vendor's JMAP role, a lowercased IMAP attribute such as `snoozed`), and sending that to
    /// another server would ask for a role the engine cannot say exists there.
    ///
    /// Named `jmap_role`, not `as_jmap_role`: that name used to return the stable name
    /// ([`as_str`](Self::as_str)), and a caller written for it must fail to build rather than
    /// quietly get `None` for the engine's own roles.
    #[must_use]
    pub fn jmap_role(&self) -> Option<&'static str> {
        Some(match self {
            Self::Inbox => "inbox",
            Self::Archive => "archive",
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::All => "all",
            Self::Flagged => "flagged",
            Self::Important => "important",
            Self::ServerRecords | Self::ChatHistory | Self::Other(_) => return None,
        })
    }
}

impl fmt::Display for MailboxRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reads the stable name back ([`MailboxRole::as_str`]); serde's form.
impl From<String> for MailboxRole {
    fn from(value: String) -> Self {
        Self::from_stable_name(&value)
    }
}

impl From<MailboxRole> for String {
    fn from(value: MailboxRole) -> Self {
        value.as_str().to_owned()
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

    /// Every role a variant of its own names, with the stable name a store has always kept.
    const STABLE_NAMES: [(MailboxRole, &str); 11] = [
        (MailboxRole::Inbox, "inbox"),
        (MailboxRole::Archive, "archive"),
        (MailboxRole::Drafts, "drafts"),
        (MailboxRole::Sent, "sent"),
        (MailboxRole::Trash, "trash"),
        (MailboxRole::Junk, "junk"),
        (MailboxRole::All, "all"),
        (MailboxRole::Flagged, "flagged"),
        (MailboxRole::Important, "important"),
        (MailboxRole::ServerRecords, "server-records"),
        (MailboxRole::ChatHistory, "chat-history"),
    ];

    #[test]
    fn every_role_keeps_the_stable_name_and_serialized_form_it_always_had() {
        let mut roles = STABLE_NAMES.to_vec();
        roles.push((MailboxRole::Other("snoozed".into()), "snoozed"));
        for (role, name) in roles {
            assert_eq!(role.as_str(), name);
            assert_eq!(role.to_string(), name);
            assert_eq!(String::from(role.clone()), name);
            // Byte for byte what a store wrote before the name and the JMAP role were apart.
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<MailboxRole>(&json).unwrap(), role);
            assert_eq!(MailboxRole::from(name.to_owned()), role);
        }
        // Read back ignoring case, as before.
        assert_eq!(
            MailboxRole::from("Chat-History".to_owned()),
            MailboxRole::ChatHistory
        );
        assert_eq!(MailboxRole::from("SENT".to_owned()), MailboxRole::Sent);
    }

    #[test]
    fn only_a_registered_role_has_a_jmap_role_to_send() {
        for (role, name) in &STABLE_NAMES[..9] {
            assert_eq!(role.jmap_role(), Some(*name));
            assert_eq!(MailboxRole::from_jmap_role(name), *role);
        }
        for role in [
            MailboxRole::ServerRecords,
            MailboxRole::ChatHistory,
            // A vendor's role, or an IMAP attribute no registry names, read from some server.
            MailboxRole::Other("snoozed".into()),
        ] {
            assert_eq!(role.jmap_role(), None, "{role}");
        }
    }

    #[test]
    fn a_server_cannot_report_the_engines_own_roles() {
        // A JMAP server that reports these names made them up; they are not the engine's.
        for name in ["server-records", "chat-history", "Server-Records"] {
            assert_eq!(
                MailboxRole::from_jmap_role(name),
                MailboxRole::Other(name.to_ascii_lowercase())
            );
        }
        // While a store's copy of the same name still reads back as the engine's role.
        assert_eq!(
            MailboxRole::from("server-records".to_owned()),
            MailboxRole::ServerRecords
        );
    }

    #[test]
    fn a_store_round_trip_is_by_name_so_an_other_named_like_a_role_comes_back_as_it() {
        // Documented on `as_str`: what a server called `server-records`, and an IMAP `\Inbox`
        // attribute, are each the role the engine means by that name once stored.
        for (reported, stored_as) in [
            (
                MailboxRole::from_jmap_role("server-records"),
                MailboxRole::ServerRecords,
            ),
            (
                MailboxRole::from_imap_special_use("\\Inbox"),
                MailboxRole::Inbox,
            ),
        ] {
            assert!(matches!(reported, MailboxRole::Other(_)), "{reported:?}");
            let json = serde_json::to_string(&reported).unwrap();
            assert_eq!(
                serde_json::from_str::<MailboxRole>(&json).unwrap(),
                stored_as
            );
        }
    }

    #[test]
    fn the_roles_no_registry_names_have_no_special_use() {
        for role in [MailboxRole::ServerRecords, MailboxRole::ChatHistory] {
            let name = role.as_str();
            assert_eq!(role.as_imap_special_use(), None, "{name}");
            assert_eq!(
                MailboxRole::from_imap_special_use(&format!("\\{name}")),
                MailboxRole::Other(name.into())
            );
        }
        // A name no role has still reads as one the engine does not know.
        assert_eq!(
            serde_json::from_str::<MailboxRole>("\"snoozed\"").unwrap(),
            MailboxRole::Other("snoozed".into())
        );
    }
}
