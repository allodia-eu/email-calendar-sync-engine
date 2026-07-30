//! The caller's access rights on a mail collection.

use serde::{Deserialize, Serialize};

/// The caller's normalized access rights on a [`Mailbox`](super::Mailbox).
///
/// Normalizes JMAP `MailboxRights` (RFC 8621 §2) and the IMAP ACL rights letters
/// (RFC 4314 §2.1) onto one set of booleans, mirroring how
/// [`CalendarAccess`](crate::calendar::CalendarAccess) normalizes calendar rights.
///
/// **Rights belong here, on the collection, not on the account.** Live against Stalwart,
/// an account shared read-only reports `accounts.<id>.isReadOnly: false` in the JMAP
/// session while the single mailbox it exposes grants only `lr` — so an account-level flag
/// cannot answer "may I write here?", and every write decision is per collection anyway.
///
/// The nine JMAP rights are carried verbatim rather than collapsed, because they are
/// genuinely independent: IMAP distinguishes `i` (append) from `t` (delete a message)
/// from `k` (create a child mailbox) from `x` (delete the mailbox), and a server hands out
/// any subset. [`may_share`] is the tenth — JMAP sharing, the IMAP `a` (administer) right,
/// and DAV's `write-acl` — and is symmetric with
/// [`CalendarAccess::may_share`](crate::calendar::CalendarAccess::may_share).
///
/// [`may_share`]: MailboxAccess::may_share
// Independent permission flags mirroring the provider rights model (JMAP `MailboxRights`
// is itself a set of booleans), not a state an enum would express better.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent permission flags, not state-machine state"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxAccess {
    /// May read the messages in this collection (JMAP `mayReadItems`; IMAP `r` plus the
    /// `l` needed to see the mailbox at all).
    pub may_read_items: bool,
    /// May add messages to this collection — an IMAP `APPEND`, or a JMAP move *into* it
    /// (`mayAddItems`; IMAP `i`).
    pub may_add_items: bool,
    /// May remove messages from this collection — a move *out* of it, or an expunge
    /// (`mayRemoveItems`; IMAP `t` for per-message delete, `e` for expunge).
    pub may_remove_items: bool,
    /// May change the `$seen` keyword on messages here (`maySetSeen`; IMAP `s`).
    ///
    /// Separate from [`may_set_keywords`](Self::may_set_keywords) because both protocols
    /// separate them: a shared mailbox is often readable with per-user seen state while
    /// other flags stay the owner's.
    pub may_set_seen: bool,
    /// May change keywords other than `$seen` (`maySetKeywords`; IMAP `w`).
    pub may_set_keywords: bool,
    /// May create a child collection under this one (`mayCreateChild`; IMAP `k`).
    pub may_create_child: bool,
    /// May rename this collection or move it under a different parent (`mayRename`).
    ///
    /// IMAP folds rename into the delete right (RFC 4314 §4: `x` on the source), so an
    /// IMAP adapter sources this from `x`.
    pub may_rename: bool,
    /// May delete this collection itself (`mayDelete`; IMAP `x`).
    pub may_delete: bool,
    /// May submit mail as this collection's owner (`maySubmit`; IMAP `p` — post).
    pub may_submit: bool,
    /// May change who else can access this collection (JMAP sharing; IMAP `a` —
    /// administer; DAV `write-acl`).
    pub may_share: bool,
}

impl MailboxAccess {
    /// Every right, as the collection's owner.
    ///
    /// The correct answer — not an optimistic one — wherever a provider grants access
    /// all-or-nothing per mailbox rather than per right: Microsoft Graph's Full Access,
    /// and Gmail labels, which carry no rights at all.
    #[must_use]
    pub fn owner() -> Self {
        Self {
            may_read_items: true,
            may_add_items: true,
            may_remove_items: true,
            may_set_seen: true,
            may_set_keywords: true,
            may_create_child: true,
            may_rename: true,
            may_delete: true,
            may_submit: true,
            may_share: true,
        }
    }

    /// Read-only: the messages are visible and nothing may be changed — the IMAP `lr`
    /// grant a mailbox shared for reading carries.
    #[must_use]
    pub fn reader() -> Self {
        Self {
            may_read_items: true,
            may_add_items: false,
            may_remove_items: false,
            may_set_seen: false,
            may_set_keywords: false,
            may_create_child: false,
            may_rename: false,
            may_delete: false,
            may_submit: false,
            may_share: false,
        }
    }
}

impl Default for MailboxAccess {
    /// [`owner`](Self::owner) — matching [`CalendarAccess`](crate::calendar::CalendarAccess),
    /// and correct for the overwhelmingly common case of a credential's own mailbox.
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
        assert!(owner.may_rename && owner.may_delete && owner.may_submit && owner.may_share);

        let reader = MailboxAccess::reader();
        assert!(reader.may_read_items);
        // Every mutation right is withheld, including the two that a "read-only" mailbox
        // is most often assumed to still allow.
        assert!(!reader.may_set_seen && !reader.may_set_keywords);
        assert!(!reader.may_add_items && !reader.may_remove_items && !reader.may_delete);
        assert!(!reader.may_create_child && !reader.may_rename);
        assert!(!reader.may_submit && !reader.may_share);
    }

    #[test]
    fn rights_roundtrip_through_json() {
        let access = MailboxAccess {
            may_set_seen: true,
            ..MailboxAccess::reader()
        };
        let json = serde_json::to_string(&access).unwrap();
        assert_eq!(
            serde_json::from_str::<MailboxAccess>(&json).unwrap(),
            access
        );
        // Per-user seen state on an otherwise read-only shared mailbox is exactly why
        // `may_set_seen` is its own right.
        assert!(access.may_set_seen && !access.may_set_keywords);
    }
}
