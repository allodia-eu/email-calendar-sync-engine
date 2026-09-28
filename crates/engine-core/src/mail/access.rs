//! The caller's access rights on a mail collection.

use serde::{Deserialize, Serialize};

/// The caller's normalized access rights on a [`Mailbox`](super::Mailbox).
///
/// Normalizes JMAP `MailboxRights` (RFC 8621 §2) and the IMAP ACL rights letters
/// (RFC 4314 §2.1) onto one set of booleans, the way
/// [`CalendarAccess`](crate::calendar::CalendarAccess) normalizes calendar rights.
///
/// **Rights live here, on the collection, and not on the account**, because the account is
/// the wrong level to ask. Live against Stalwart, a JMAP account shared read-only reports
/// `accounts.<id>.isReadOnly: false` while the single mailbox it exposes grants only
/// `lr`. A share can be granted folder by folder, so "may I do this here?" has an answer
/// only per collection.
///
/// The rights are kept separate rather than collapsed to read/write because servers hand
/// out arbitrary subsets of them: a shared mailbox commonly lets each reader keep their own
/// `$seen` state ([`may_set_seen`](Self::may_set_seen)) without changing any other keyword,
/// and IMAP separates adding a message (`i`) from removing one (`t`+`e`) from creating a
/// child (`k`) from deleting the mailbox (`x`).
///
/// # What is not here, on purpose
///
/// JMAP's ninth right, `maySubmit`, and the IMAP `p` it mirrors, mean that mail may be
/// **posted into** this mailbox by address (RFC 8621 §2: "Messages may be submitted
/// directly to this Mailbox"; RFC 4314: "send mail to submission address for mailbox").
/// No client action depends on that, and the name invites the reading that it grants
/// sending *as* the mailbox's owner — which no mailbox right expresses. A field that
/// could only be misread is left out.
// Independent permission flags mirroring the provider rights model (JMAP `MailboxRights`
// is itself a set of booleans), not a state an enum would express better.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent permission flags, not state-machine state"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxAccess {
    /// May read the messages in this collection (JMAP `mayReadItems`; IMAP `l` **and**
    /// `r`, because `r` grants the reads while `l` is what makes the mailbox visible).
    pub may_read_items: bool,
    /// May add messages to this collection: an `APPEND`, a copy or move *into* it, or a
    /// draft saved here (JMAP `mayAddItems`; IMAP `i`).
    pub may_add_items: bool,
    /// May take messages out of this collection: a move *out*, or a delete (JMAP
    /// `mayRemoveItems`; IMAP `t` **and** `e` — setting `\Deleted` without the right to
    /// expunge leaves the message where it was, and RFC 6851 `MOVE` needs both on the
    /// source).
    pub may_remove_items: bool,
    /// May change the `$seen` keyword on messages here (JMAP `maySetSeen`; IMAP `s`).
    ///
    /// Separate from [`may_set_keywords`](Self::may_set_keywords) because both protocols
    /// separate them: a shared mailbox is often readable with per-reader seen state while
    /// every other flag stays the owner's.
    pub may_set_seen: bool,
    /// May change keywords other than `$seen` — flagging, answering, labels (JMAP
    /// `maySetKeywords`; IMAP `w`).
    pub may_set_keywords: bool,
    /// May create a child collection under this one (JMAP `mayCreateChild`; IMAP `k`).
    pub may_create_child: bool,
    /// May rename this collection or move it under another parent (JMAP `mayRename`;
    /// IMAP `x`, because RFC 4314 §4 folds renaming into the right to delete the old name —
    /// the `k` it also needs belongs to the *new* parent, not to this mailbox).
    pub may_rename: bool,
    /// May delete this collection itself (JMAP `mayDelete`; IMAP `x`).
    pub may_delete: bool,
    /// May change who else can access this collection (IMAP `a`, *administer*; JMAP
    /// `mayShare`, which RFC 8621 does not define but servers implementing sharing
    /// report, and which is read as withheld where absent).
    pub may_share: bool,
}

impl MailboxAccess {
    /// Every right, as the collection's owner.
    ///
    /// The right answer — not an optimistic one — wherever the protocol has no per-mailbox
    /// rights to report: a server without the IMAP `ACL` extension offers no sharing to be
    /// restricted by, and Gmail labels carry no rights at all. Also what a stored mailbox
    /// that predates rights deserializes as (see [`Mailbox`](super::Mailbox)).
    #[must_use]
    pub const fn owner() -> Self {
        Self {
            may_read_items: true,
            may_add_items: true,
            may_remove_items: true,
            may_set_seen: true,
            may_set_keywords: true,
            may_create_child: true,
            may_rename: true,
            may_delete: true,
            may_share: true,
        }
    }

    /// Read-only: the messages are visible and nothing may be changed, not even `$seen` —
    /// the IMAP `lr` grant, and a JMAP `myRights` with `mayReadItems` alone.
    #[must_use]
    pub const fn reader() -> Self {
        Self {
            may_read_items: true,
            may_add_items: false,
            may_remove_items: false,
            may_set_seen: false,
            may_set_keywords: false,
            may_create_child: false,
            may_rename: false,
            may_delete: false,
            may_share: false,
        }
    }
}

impl Default for MailboxAccess {
    /// [`owner`](Self::owner), matching [`CalendarAccess`](crate::calendar::CalendarAccess).
    fn default() -> Self {
        Self::owner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_may_do_everything_and_a_reader_only_read() {
        let owner = MailboxAccess::owner();
        assert_eq!(owner, MailboxAccess::default());
        assert!(owner.may_read_items && owner.may_add_items && owner.may_remove_items);
        assert!(owner.may_set_seen && owner.may_set_keywords && owner.may_create_child);
        assert!(owner.may_rename && owner.may_delete && owner.may_share);

        let reader = MailboxAccess::reader();
        assert!(reader.may_read_items);
        // Every mutation is withheld, including the two a "read-only" mailbox is most often
        // assumed to still allow.
        assert!(!reader.may_set_seen && !reader.may_set_keywords);
        assert!(!reader.may_add_items && !reader.may_remove_items && !reader.may_delete);
        assert!(!reader.may_create_child && !reader.may_rename && !reader.may_share);
    }

    #[test]
    fn rights_roundtrip_through_json() {
        // Per-reader seen state on an otherwise read-only mailbox is exactly why
        // `may_set_seen` is its own right, so it is the shape that must survive storage.
        let access = MailboxAccess {
            may_set_seen: true,
            ..MailboxAccess::reader()
        };
        let json = serde_json::to_string(&access).unwrap();
        assert_eq!(
            serde_json::from_str::<MailboxAccess>(&json).unwrap(),
            access
        );
    }
}
