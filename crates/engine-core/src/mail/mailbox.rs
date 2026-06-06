//! Mail collections (mailboxes, folders, labels).

use serde::{Deserialize, Serialize};

use super::MailboxRole;
use crate::extended::ExtendedProperties;
use crate::ids::MailboxId;
use crate::version::RevisionTokens;

/// A mail collection: a mailbox, folder, or label.
///
/// Identity ([`MailboxId`]), normalized [`role`](MailboxRole), and display name
/// are three separate things. Membership of messages in this collection is
/// modeled on the message side, not here. Per-mailbox access rights and message
/// counts are provider-specific and, when needed, carried in
/// [`extended`](Mailbox::extended) rather than asserted as universal fields.
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
    /// Per-object revision tokens, if the provider supplies any.
    pub revisions: RevisionTokens,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

impl Mailbox {
    /// Creates a top-level mailbox with the given id and name, no role, and
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
