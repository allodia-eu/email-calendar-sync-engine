//! Mail collections (mailboxes, folders, labels).

use serde::{Deserialize, Serialize};

use super::{MailboxAccess, MailboxRole};
use crate::{extended::ExtendedProperties, ids::MailboxId, version::RevisionTokens};

/// A mail collection: a mailbox, folder, or label.
///
/// Identity ([`MailboxId`]), normalized [`role`](MailboxRole), and display name
/// are three separate things. Membership of messages in this collection is
/// modeled on the message side, not here.
///
/// The caller's **access rights** are a universal field ([`access`](Mailbox::access)),
/// not a provider extra: both mail protocols that share mailboxes standardise them (JMAP
/// `MailboxRights`, RFC 8621 §2; IMAP ACL, RFC 4314), and there is no usable answer above
/// the collection — see [`MailboxAccess`] (`modeling.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mailbox {
    /// The collection's stable id.
    pub id: MailboxId,
    /// The display name.
    pub name: String,
    /// The parent collection for hierarchical providers; `None` at the top
    /// level.
    pub parent: Option<MailboxId>,
    /// The normalized role, if this collection has one.
    pub role: Option<MailboxRole>,
    /// A provider-supplied sort hint (JMAP `sortOrder`); lower sorts first.
    pub sort_order: u32,
    /// Whether the user is subscribed to this collection (IMAP subscription).
    pub subscribed: bool,
    /// How many messages in this collection are unread, as **the server counts
    /// them** — not as the synced window does. `None` means the provider did not
    /// report one, which a host must not read as zero.
    ///
    /// Server-side rather than derived because a store holds only the synced
    /// window (a host may sync three months of a mailbox holding years), so
    /// counting rows answers a different question than the one a folder badge
    /// asks. All four mail transports report it: JMAP `unreadEmails`, IMAP
    /// `STATUS (UNSEEN)`, Gmail `messagesUnread`, Graph `unreadItemCount`.
    ///
    /// Counts **messages**, not conversations — JMAP alone offers the
    /// conversation form (`unreadThreads`), so a portable field cannot mean that.
    #[serde(default)]
    pub unread_count: Option<u32>,
    /// What the caller may do in this collection.
    ///
    /// `#[serde(default)]`, so a mailbox stored before rights existed still loads — as
    /// [`owner`](MailboxAccess::owner), which is what every caller implicitly assumed while
    /// there was no field. `NORMALIZER_VERSION` 6 re-snapshots such rows, so the default is
    /// what a row says only until the next sync replaces it with the server's answer.
    #[serde(default)]
    pub access: MailboxAccess,
    /// Per-object revision tokens, if the provider supplies any.
    pub revisions: RevisionTokens,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

impl Mailbox {
    /// Creates a top-level mailbox with the given id and name, no role, owner rights, and
    /// default metadata.
    #[must_use]
    pub fn new(id: MailboxId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
            unread_count: None,
            access: MailboxAccess::owner(),
            revisions: RevisionTokens::none(),
            extended: ExtendedProperties::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> MailboxId {
        MailboxId::try_from(value).unwrap()
    }

    #[test]
    fn new_mailbox_has_sensible_defaults() {
        let mailbox = Mailbox::new(id("inbox"), "Inbox");
        assert_eq!(mailbox.name, "Inbox");
        assert!(mailbox.parent.is_none());
        assert!(mailbox.role.is_none());
        assert!(mailbox.subscribed);
        // Absent, not zero: a mailbox nobody has counted yet is not an empty one.
        assert!(mailbox.unread_count.is_none());
        // Owner rights: the constructor builds a credential's own folder, and an adapter
        // that learns narrower rights overwrites the field.
        assert_eq!(mailbox.access, MailboxAccess::owner());
    }

    #[test]
    fn unread_count_roundtrips_and_a_payload_without_it_still_loads() {
        let mut counted = Mailbox::new(id("inbox"), "Inbox");
        counted.unread_count = Some(545);
        let json = serde_json::to_string(&counted).unwrap();
        assert_eq!(serde_json::from_str::<Mailbox>(&json).unwrap(), counted);

        // A row stored before this field existed deserializes to "not reported"
        // rather than failing the read — the store is JSON payloads, so this is
        // what spares the schema a migration.
        let stored_earlier = serde_json::to_string(&serde_json::json!({
            "id": "inbox",
            "name": "Inbox",
            "parent": null,
            "role": null,
            "sort_order": 0,
            "subscribed": true,
            "revisions": RevisionTokens::none(),
            "extended": ExtendedProperties::new(),
        }))
        .unwrap();
        let loaded: Mailbox = serde_json::from_str(&stored_earlier).unwrap();
        assert!(loaded.unread_count.is_none());
        // The same row predates rights too, and reads as what callers assumed before there
        // was a field: the owner's.
        assert_eq!(loaded.access, MailboxAccess::owner());
    }

    #[test]
    fn narrower_rights_survive_the_json_roundtrip() {
        // A shared, read-only mailbox is the case the field exists for: it must survive
        // being stored and read back, since that is how a host learns not to offer a write.
        let mut shared = Mailbox::new(id("Shared Folders/bob@test.local/INBOX"), "INBOX");
        shared.access = MailboxAccess::reader();
        let json = serde_json::to_string(&shared).unwrap();
        let back: Mailbox = serde_json::from_str(&json).unwrap();
        assert_eq!(back, shared);
        assert!(back.access.may_read_items && !back.access.may_add_items);
    }

    #[test]
    fn hierarchy_and_role_roundtrip() {
        let mut child = Mailbox::new(id("work/clients"), "Clients");
        child.parent = Some(id("work"));
        child.role = Some(MailboxRole::Archive);
        let json = serde_json::to_string(&child).unwrap();
        assert_eq!(serde_json::from_str::<Mailbox>(&json).unwrap(), child);
    }
}
