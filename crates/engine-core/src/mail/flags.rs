//! The system keywords a mailbox list reads, packed into one integer.

use serde::{Deserialize, Serialize};

use super::{Keyword, SystemKeyword};

/// The RFC 8621 system keywords a list row is drawn from, as a bitfield.
///
/// Keywords are an open set — a user keyword is any string — so the full set lives in the
/// membership junction, where an arbitrary-cardinality set belongs. These four are the ones a
/// row's appearance depends on, and a list read must not pay a join to learn them: they sit in
/// the message row itself, one integer wide.
///
/// The bit positions are persisted, so they are append-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MailFlags(u32);

impl MailFlags {
    const ANSWERED: u32 = 1 << 3;
    const DRAFT: u32 = 1 << 2;
    const FLAGGED: u32 = 1 << 1;
    const SEEN: u32 = 1 << 0;

    /// Collects the recognised system keywords out of a message's keyword set.
    pub fn from_keywords<'a>(keywords: impl IntoIterator<Item = &'a Keyword>) -> Self {
        let mut bits = 0;
        for keyword in keywords {
            bits |= match keyword.as_system() {
                Some(SystemKeyword::Seen) => Self::SEEN,
                Some(SystemKeyword::Flagged) => Self::FLAGGED,
                Some(SystemKeyword::Draft) => Self::DRAFT,
                Some(SystemKeyword::Answered) => Self::ANSWERED,
                _ => 0,
            };
        }
        Self(bits)
    }

    /// Rebuilds the set from its stored representation, ignoring bits this build does not know.
    #[must_use]
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The stored representation.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether `$seen` is set.
    #[must_use]
    pub fn seen(self) -> bool {
        self.0 & Self::SEEN != 0
    }

    /// Whether `$flagged` is set.
    #[must_use]
    pub fn flagged(self) -> bool {
        self.0 & Self::FLAGGED != 0
    }

    /// Whether `$draft` is set.
    #[must_use]
    pub fn draft(self) -> bool {
        self.0 & Self::DRAFT != 0
    }

    /// Whether `$answered` is set.
    #[must_use]
    pub fn answered(self) -> bool {
        self.0 & Self::ANSWERED != 0
    }

    /// Whether the message counts as unread.
    ///
    /// Per RFC 8621 §2 a message is unread when it has neither `$seen` nor `$draft` — a draft is
    /// never "unread". Mirrors [`Message::is_unread`](super::Message::is_unread).
    #[must_use]
    pub fn is_unread(self) -> bool {
        !self.seen() && !self.draft()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn flags(keywords: &[Keyword]) -> MailFlags {
        MailFlags::from_keywords(keywords)
    }

    #[test]
    fn each_system_keyword_lands_on_its_own_bit() {
        assert!(flags(&[Keyword::system(SystemKeyword::Seen)]).seen());
        assert!(flags(&[Keyword::system(SystemKeyword::Flagged)]).flagged());
        assert!(flags(&[Keyword::system(SystemKeyword::Draft)]).draft());
        assert!(flags(&[Keyword::system(SystemKeyword::Answered)]).answered());
        let one = flags(&[Keyword::system(SystemKeyword::Seen)]);
        assert!(!one.flagged() && !one.draft() && !one.answered());
    }

    #[test]
    fn unrecognised_keywords_contribute_nothing() {
        let set: BTreeSet<Keyword> = [
            Keyword::new("project-x").unwrap(),
            Keyword::system(SystemKeyword::Junk),
        ]
        .into_iter()
        .collect();
        assert_eq!(MailFlags::from_keywords(&set), MailFlags::default());
    }

    #[test]
    fn a_draft_is_not_unread_but_an_unseen_message_is() {
        assert!(MailFlags::default().is_unread());
        assert!(!flags(&[Keyword::system(SystemKeyword::Seen)]).is_unread());
        assert!(!flags(&[Keyword::system(SystemKeyword::Draft)]).is_unread());
    }

    #[test]
    fn bits_round_trip_through_storage() {
        let set = flags(&[
            Keyword::system(SystemKeyword::Seen),
            Keyword::system(SystemKeyword::Answered),
        ]);
        assert_eq!(MailFlags::from_bits(set.bits()), set);
    }

    #[test]
    fn an_unknown_stored_bit_is_preserved_and_reads_as_no_known_flag() {
        // A database written by a later build carries bits this one has no name for.
        let future = MailFlags::from_bits(1 << 31);
        assert!(!future.seen() && !future.flagged() && !future.draft() && !future.answered());
        assert_eq!(future.bits(), 1 << 31);
    }
}
