//! The IMAP `MYRIGHTS` response (RFC 4314 §3.8) and its rights letters (§2.1), mapped
//! onto the engine's [`MailboxAccess`].
//!
//! IMAP states rights as a string of single letters, and the mapping to the engine's named
//! booleans is not one-to-one — so each one is spelled out below with the RFC's own
//! definition, because guessing at any of them silently offers a write the server will
//! refuse (or hides one it would allow).
//!
//! | Letter | RFC 4314 §2.1 meaning | Engine right |
//! |---|---|---|
//! | `l` | lookup: the mailbox is visible to `LIST`/`LSUB` | (prerequisite for read) |
//! | `r` | read: `SELECT`, `FETCH`, `SEARCH`, `COPY` from | [`may_read_items`] |
//! | `s` | keep `\Seen` across sessions | [`may_set_seen`] |
//! | `w` | write flags other than `\Seen`/`\Deleted` | [`may_set_keywords`] |
//! | `i` | insert: `APPEND`, `COPY` into | [`may_add_items`] |
//! | `p` | post: send mail to the mailbox's submission address | [`may_submit`] |
//! | `k` | create a child mailbox | [`may_create_child`] |
//! | `x` | delete the mailbox (and rename it away) | [`may_delete`], [`may_rename`] |
//! | `t` | delete messages: set `\Deleted` | (with `e`) [`may_remove_items`] |
//! | `e` | `EXPUNGE` | (with `t`) [`may_remove_items`] |
//! | `a` | administer: `SETACL` on the mailbox | [`may_share`] |
//!
//! Three of those deserve their reasoning stated:
//!
//! - **Reading needs `l` *and* `r`.** `r` alone grants the operations but `l` is what makes the
//!   mailbox visible at all, so a grant of one without the other is not a readable mailbox.
//! - **Removing a message needs `t` *and* `e`.** Taking a message out of an IMAP folder is "set
//!   `\Deleted`, then expunge" — and RFC 6851's `MOVE` requires the same pair on the source. Either
//!   letter alone leaves the message still there.
//! - **Renaming maps to `x`.** RFC 4314 §4 requires `x` on the *old* name plus `k` on the new
//!   parent. Only the first is a right *of this mailbox*; the second belongs to whichever mailbox
//!   the caller renames into, so it cannot be answered here.
//!
//! [`may_read_items`]: MailboxAccess::may_read_items
//! [`may_set_seen`]: MailboxAccess::may_set_seen
//! [`may_set_keywords`]: MailboxAccess::may_set_keywords
//! [`may_add_items`]: MailboxAccess::may_add_items
//! [`may_submit`]: MailboxAccess::may_submit
//! [`may_create_child`]: MailboxAccess::may_create_child
//! [`may_delete`]: MailboxAccess::may_delete
//! [`may_rename`]: MailboxAccess::may_rename
//! [`may_remove_items`]: MailboxAccess::may_remove_items
//! [`may_share`]: MailboxAccess::may_share

use engine_core::mail::MailboxAccess;

use crate::tokenize::items_of;

/// One mailbox's rights, as the server reported them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxRights {
    /// The mailbox the rights are for, as the server named it.
    pub(crate) mailbox: String,
    /// The rights letters, verbatim (e.g. `"lr"`, `"rliteswkxpa"`).
    pub(crate) letters: String,
}

impl MailboxRights {
    /// Whether the grant includes `letter`.
    ///
    /// Case-sensitive, because RFC 4314 §2.1.1 makes case significant: the standard rights
    /// are lowercase, and an uppercase letter is reserved for a server-specific extension
    /// that must not be mistaken for one of them.
    fn has(&self, letter: char) -> bool {
        self.letters.contains(letter)
    }

    /// The rights as the engine's named booleans. See this module's table for each mapping
    /// and the reasoning behind the three that combine letters.
    pub(crate) fn access(&self) -> MailboxAccess {
        MailboxAccess {
            may_read_items: self.has('l') && self.has('r'),
            may_add_items: self.has('i'),
            may_remove_items: self.has('t') && self.has('e'),
            may_set_seen: self.has('s'),
            may_set_keywords: self.has('w'),
            may_create_child: self.has('k'),
            may_rename: self.has('x'),
            may_delete: self.has('x'),
            may_submit: self.has('p'),
            may_share: self.has('a'),
        }
    }
}

/// Parses the untagged `* MYRIGHTS <mailbox> <rights>` response (RFC 4314 §3.8).
///
/// `None` when no line in the response is a `MYRIGHTS` — which a server without the ACL
/// extension is entitled to, and which the caller reads as "rights unknown" rather than as
/// "no rights".
pub(crate) fn parse_myrights(lines: &[Vec<u8>]) -> Option<MailboxRights> {
    for line in lines {
        let Ok(items) = items_of(line) else { continue };
        let [keyword, mailbox, rights, ..] = items.as_slice() else {
            continue;
        };
        if !keyword
            .as_atom()
            .is_some_and(|atom| atom.eq_ignore_ascii_case("MYRIGHTS"))
        {
            continue;
        }
        return Some(MailboxRights {
            mailbox: mailbox.as_nstring()?,
            letters: rights.as_nstring()?,
        });
    }
    None
}

#[cfg(test)]
#[path = "acl_tests.rs"]
mod tests;
