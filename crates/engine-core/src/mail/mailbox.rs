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
/// Per-mailbox **access rights** are a universal field ([`access`](Mailbox::access)) — a
/// reversal of the earlier decision to leave them in
/// [`extended`](Mailbox::extended) as provider-specific. Two of the three mail protocols
/// that support sharing standardise them (JMAP `MailboxRights`, RFC 8621 §2; IMAP ACL,
/// RFC 4314), the third grants them all-or-nothing per mailbox, and the alternative
/// signal is unusable: a JMAP account shared read-only reports `isReadOnly: false` while
/// its only mailbox grants read alone. A caller deciding "may I write here?" therefore has
/// no answer above the collection, which makes rights structural rather than a provider
/// extra (`modeling.md`). Message **counts** remain provider-specific and stay in
/// `extended`.
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
    /// The caller's rights on this collection.
    ///
    /// `#[serde(default)]` so a mailbox stored before rights existed still deserializes —
    /// as [`owner`](MailboxAccess::owner), which is what a caller *implicitly* assumed
    /// when there was no field, rather than as an error that would make the row
    /// unreadable. `NORMALIZER_VERSION` 4 clears the sync cursors, so the next snapshot
    /// replaces such a row with the server's real rights.
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
        // Owner rights: the constructor is used for a credential's own folders, and an
        // adapter that learns narrower rights overwrites the field.
        assert_eq!(mailbox.access, MailboxAccess::owner());
    }

    #[test]
    fn hierarchy_and_role_roundtrip() {
        let mut child = Mailbox::new(id("work/clients"), "Clients");
        child.parent = Some(id("work"));
        child.role = Some(MailboxRole::Archive);
        let json = serde_json::to_string(&child).unwrap();
        assert_eq!(serde_json::from_str::<Mailbox>(&json).unwrap(), child);
    }

    #[test]
    fn a_mailbox_stored_before_rights_existed_still_deserializes() {
        // The pre-`NORMALIZER_VERSION` 4 payload shape: no `access` key at all. It must
        // read back rather than erroring, because such rows are still in the store when it
        // is opened — the version bump clears the cursors, but the re-snapshot that
        // replaces them has not run yet.
        let legacy = serde_json::json!({
            "id": "INBOX",
            "name": "Inbox",
            "parent": null,
            "role": "inbox",
            "sort_order": 0,
            "subscribed": true,
            "revisions": {},
            "extended": {},
        });
        let mailbox: Mailbox = serde_json::from_value(legacy).expect("legacy payload loads");
        assert_eq!(mailbox.access, MailboxAccess::owner());
    }

    #[test]
    fn narrower_rights_survive_the_json_roundtrip() {
        // A shared, read-only mailbox is the case the field exists for: it must survive
        // being stored and re-read, since that is how a host learns not to offer a write.
        let mut shared = Mailbox::new(id("Shared Folders/bob@test.local/INBOX"), "INBOX");
        shared.access = MailboxAccess::reader();
        let json = serde_json::to_string(&shared).unwrap();
        let back: Mailbox = serde_json::from_str(&json).unwrap();
        assert_eq!(back, shared);
        assert!(back.access.may_read_items && !back.access.may_add_items);
    }
}
