//! The keyword change a queued op makes, which the store shows before the server confirms it.
//!
//! A mark-read or a flag is an op the outbox may hold for a while: until a connection returns, a
//! rate limit lifts, or a retry comes round. What the user sees meanwhile is the change they made,
//! so the store shows it from the moment it is queued. The server stays the authority: the store
//! keeps the keywords the server last reported beside the message and shows them with every
//! queued change applied on top, so a sync updates the server's half, an accepted change joins it,
//! and one the server refuses for good drops out of what is shown.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{ids::ProviderKey, mail::Keyword};

/// Keywords a queued op sets and clears on one message.
///
/// Expressed as a change rather than a result, so several queued changes to one message compose
/// in the order they were queued and each survives whatever the server reports about the
/// keywords it does not touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordEdit {
    /// The message the change applies to.
    pub key: ProviderKey,
    /// Keywords to set.
    pub add: BTreeSet<Keyword>,
    /// Keywords to clear.
    pub remove: BTreeSet<Keyword>,
}

impl KeywordEdit {
    /// A change to `key` that sets `add` and clears `remove`.
    #[must_use]
    pub fn new(key: ProviderKey, add: BTreeSet<Keyword>, remove: BTreeSet<Keyword>) -> Self {
        Self { key, add, remove }
    }

    /// Applies the change to `keywords`.
    pub fn apply_to(&self, keywords: &mut BTreeSet<Keyword>) {
        for keyword in &self.remove {
            keywords.remove(keyword);
        }
        keywords.extend(self.add.iter().cloned());
    }

    /// `server` with every change in `edits` applied, in order: what the store shows.
    #[must_use]
    pub fn shown<'a>(
        server: &BTreeSet<Keyword>,
        edits: impl IntoIterator<Item = &'a Self>,
    ) -> BTreeSet<Keyword> {
        let mut shown = server.clone();
        for edit in edits {
            edit.apply_to(&mut shown);
        }
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::SystemKeyword;

    fn key() -> ProviderKey {
        ProviderKey::new("m1").unwrap()
    }

    fn set(keywords: &[SystemKeyword]) -> BTreeSet<Keyword> {
        keywords.iter().map(|&k| Keyword::system(k)).collect()
    }

    #[test]
    fn queued_changes_apply_in_order_over_what_the_server_reported() {
        let flag = KeywordEdit::new(key(), set(&[SystemKeyword::Flagged]), BTreeSet::new());
        let read = KeywordEdit::new(key(), set(&[SystemKeyword::Seen]), BTreeSet::new());
        let unflag = KeywordEdit::new(key(), BTreeSet::new(), set(&[SystemKeyword::Flagged]));

        let server = set(&[SystemKeyword::Answered]);
        assert_eq!(
            KeywordEdit::shown(&server, [&flag, &read]),
            set(&[
                SystemKeyword::Answered,
                SystemKeyword::Flagged,
                SystemKeyword::Seen
            ]),
        );
        assert_eq!(
            KeywordEdit::shown(&server, [&flag, &unflag]),
            server,
            "the later change wins over the earlier one",
        );
    }

    #[test]
    fn a_change_leaves_the_keywords_it_does_not_name_to_the_server() {
        let read = KeywordEdit::new(key(), set(&[SystemKeyword::Seen]), BTreeSet::new());
        let server = set(&[SystemKeyword::Flagged]);
        assert_eq!(
            KeywordEdit::shown(&server, [&read]),
            set(&[SystemKeyword::Flagged, SystemKeyword::Seen]),
        );
    }
}
