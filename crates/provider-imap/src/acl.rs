//! The IMAP `MYRIGHTS` answer (RFC 4314 §3.8) and its rights letters (§2.1), as the
//! engine's [`MailboxAccess`].
//!
//! Rights are single letters, and they do not map one-to-one onto named rights, so each is
//! spelled out with the RFC's own meaning — a wrong guess silently offers a write the server
//! refuses, or hides one it would allow.
//!
//! | Letter | RFC 4314 §2.1 | Engine right |
//! |---|---|---|
//! | `l` | lookup: visible to `LIST` | with `r`: [`may_read_items`] |
//! | `r` | read: `SELECT`, `FETCH`, `SEARCH`, `COPY` from | with `l`: [`may_read_items`] |
//! | `s` | keep `\Seen` across sessions | [`may_set_seen`] |
//! | `w` | write flags other than `\Seen` and `\Deleted` | [`may_set_keywords`] |
//! | `i` | insert: `APPEND`, `COPY` into | [`may_add_items`] |
//! | `k` | create a child mailbox | [`may_create_child`] |
//! | `x` | delete the mailbox, and rename it away | [`may_delete`], [`may_rename`] |
//! | `t` | set `\Deleted` on messages | with `e`: [`may_remove_items`] |
//! | `e` | `EXPUNGE` | with `t`: [`may_remove_items`] |
//! | `a` | administer: `SETACL` | [`may_share`] |
//! | `p` | post to the mailbox's submission address | — (see [`MailboxAccess`]) |
//!
//! Three are combinations, and deliberately so. Reading needs `l` **and** `r`: `r` grants the
//! operations and `l` is what makes the mailbox visible at all. Removing a message needs `t`
//! **and** `e`: flagging it `\Deleted` without the right to expunge leaves it where it was, and
//! RFC 6851 `MOVE` requires the same pair on the source. Renaming is `x` alone: RFC 4314 §4
//! also wants `k` on the *new* parent, which is a right of that mailbox, not this one.
//!
//! RFC 2086's `c` and `d` are **ignored**, and reading them as grants would be a bug. RFC 4314
//! §2.1.1 keeps them only as compatibility letters a server MUST add to a `MYRIGHTS` answer
//! when *any one* of their member rights is set: a grant of `t` alone comes back as `lrtd`,
//! and expanding that `d` to `e`, `t` and `x` would report a folder the credential may flag
//! but not expunge as one it may empty. A server implementing RFC 4314 — every one this
//! repository runs, each advertising `RIGHTS=texk` — sends the member letters themselves, so
//! the aliases carry nothing the members do not. Only a server still speaking RFC 2086 sends
//! the alias alone, and reading nothing there under-reports — hides an action rather than
//! offering one the server refuses — which is the safe direction.
//!
//! [`may_read_items`]: MailboxAccess::may_read_items
//! [`may_set_seen`]: MailboxAccess::may_set_seen
//! [`may_set_keywords`]: MailboxAccess::may_set_keywords
//! [`may_add_items`]: MailboxAccess::may_add_items
//! [`may_create_child`]: MailboxAccess::may_create_child
//! [`may_delete`]: MailboxAccess::may_delete
//! [`may_rename`]: MailboxAccess::may_rename
//! [`may_remove_items`]: MailboxAccess::may_remove_items
//! [`may_share`]: MailboxAccess::may_share

use engine_core::mail::MailboxAccess;

use crate::tokenize::items_of;

/// The rights letters as [`MailboxAccess`]. Case-sensitive: RFC 4314 §2 reserves the lower
/// case for the standard rights, allows no upper-case ones at all, and leaves digits to
/// implementations — so only a lower-case letter is ever read as a standard right.
pub(crate) fn access(letters: &str) -> MailboxAccess {
    let has = |letter: char| letters.contains(letter);
    MailboxAccess {
        may_read_items: has('l') && has('r'),
        may_add_items: has('i'),
        may_remove_items: has('t') && has('e'),
        may_set_seen: has('s'),
        may_set_keywords: has('w'),
        may_create_child: has('k'),
        may_rename: has('x'),
        may_delete: has('x'),
        may_share: has('a'),
    }
}

/// The mailbox and rights letters of one untagged `MYRIGHTS <mailbox> <rights>` line, as
/// the server wrote them, or `None` for any other line.
///
/// The mailbox is what pairs an answer with its question: a server may complete pipelined
/// commands in any order (RFC 9051 §5.5), and Stalwart does, so the tag that finishes after
/// a `* MYRIGHTS` line is not necessarily the one that asked for it.
pub(crate) fn parse_myrights(line: &[u8]) -> Option<(String, String)> {
    let items = items_of(line).ok()?;
    let [keyword, mailbox, rights, ..] = items.as_slice() else {
        return None;
    };
    if !keyword
        .as_atom()
        .is_some_and(|atom| atom.eq_ignore_ascii_case("MYRIGHTS"))
    {
        return None;
    }
    Some((mailbox.as_nstring()?, rights.as_nstring()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_grant_is_the_owners_and_lr_is_a_readers() {
        // The two grants the Stalwart fixture holds, as it answers them.
        assert_eq!(access("rliteswkxpa"), MailboxAccess::owner());
        assert_eq!(access("rl"), MailboxAccess::reader());
    }

    #[test]
    fn the_combined_rights_need_both_letters() {
        // `r` without `l` is not a readable mailbox; `t` without `e` removes nothing.
        assert!(!access("r").may_read_items);
        assert!(!access("lrt").may_remove_items);
        assert!(!access("lre").may_remove_items);
        assert!(access("lrte").may_remove_items);
        // `x` is both rename and delete; `k` alone is neither.
        let x = access("lrx");
        assert!(x.may_rename && x.may_delete && !x.may_create_child);
        let k = access("lrk");
        assert!(k.may_create_child && !k.may_rename && !k.may_delete);
    }

    #[test]
    fn each_single_right_maps_to_its_own_flag() {
        assert!(access("s").may_set_seen && !access("s").may_set_keywords);
        assert!(access("w").may_set_keywords && !access("w").may_set_seen);
        assert!(access("i").may_add_items);
        assert!(access("a").may_share);
        // Posting to the mailbox's address grants nothing a client acts on.
        assert_eq!(access("p"), access(""));
    }

    #[test]
    fn the_rfc_2086_letters_grant_nothing_their_members_do_not() {
        // RFC 4314 §2.1.1: a server adds `d` when *any* delete member is set, and `c` for any
        // create member. So `lrtd` is "may flag, may not expunge", and `lrkc` is "may create a
        // child, may not delete this". Expanding the aliases would report both as more.
        let flag_only = access("lrtd");
        assert!(!flag_only.may_remove_items && !flag_only.may_delete);
        let child_only = access("lrkc");
        assert!(child_only.may_create_child && !child_only.may_delete && !child_only.may_rename);
        // The full grant a server sends with its aliases reads the same as without them.
        assert_eq!(access("lrswipkxtecda"), access("lrswipkxtea"));
    }

    #[test]
    fn only_a_lower_case_letter_is_a_standard_right() {
        // Upper case is not allowed, and digits are an implementation's own rights.
        assert_eq!(access("LRITE"), access(""));
        assert_eq!(access("0123456789"), access(""));
    }

    #[test]
    fn a_myrights_line_names_its_mailbox_and_its_rights() {
        assert_eq!(
            parse_myrights(b"MYRIGHTS \"Shared Folders/bob@test.local/INBOX\" rl"),
            Some((
                "Shared Folders/bob@test.local/INBOX".to_owned(),
                "rl".to_owned()
            ))
        );
        // An atom mailbox name is a name too.
        assert_eq!(
            parse_myrights(b"MYRIGHTS INBOX lrswipkxtea").map(|(name, _)| name),
            Some("INBOX".to_owned())
        );
        // Anything else carries no rights at all.
        assert_eq!(parse_myrights(b"3 EXISTS"), None);
        assert_eq!(parse_myrights(b"MYRIGHTS INBOX"), None);
    }
}
