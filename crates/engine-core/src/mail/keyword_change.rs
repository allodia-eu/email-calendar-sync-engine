//! A change that moved a message's keywords and nothing else.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::Keyword;
use crate::ids::ProviderKey;

/// A message whose provider reported a **keyword change and nothing else**.
///
/// Every adapter can recognise one: an IMAP `CHANGEDSINCE` row carrying `FLAGS` with no
/// `ENVELOPE`, a Gmail `labelsAdded`/`labelsRemoved` history record, a Microsoft Graph
/// delta entry without an `@odata.etag`, a JMAP id in `Email/changes`'s `updated` rather
/// than its `created`. Each then costs one small call to read the resulting keywords,
/// instead of re-fetching a whole message.
///
/// Kept out of a sync page's `changed` list because it is *partial*: the store writes the
/// message row's flags and the message's keyword memberships, and touches nothing else —
/// not the row's other columns, not the full-text document, not the address junctions,
/// and not the normalized payload. A field the provider did not send therefore cannot be
/// destroyed by a change that never claimed to carry it.
///
/// An adapter that cannot tell a keyword change from a content change emits none of
/// these; its messages ride in `changed` as whole objects, exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailKeywordChange {
    /// The message whose keywords moved.
    pub key: ProviderKey,
    /// The message's keywords **after** the change — the complete set, never a delta.
    ///
    /// A complete set is idempotent on replay, which an add/remove pair is not: a sync
    /// that re-delivers a page must leave the same state. Every provider can supply it in
    /// one cheap call, so nothing is bought by carrying the narrower form.
    pub keywords: BTreeSet<Keyword>,
}

impl MailKeywordChange {
    /// A keyword change for `key` resulting in `keywords`.
    #[must_use]
    pub fn new(key: ProviderKey, keywords: BTreeSet<Keyword>) -> Self {
        Self { key, keywords }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::SystemKeyword;

    #[test]
    fn roundtrips_through_json() {
        let change = MailKeywordChange::new(
            ProviderKey::new("inbox/1/42").unwrap(),
            [Keyword::system(SystemKeyword::Seen)].into_iter().collect(),
        );
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(
            serde_json::from_str::<MailKeywordChange>(&json).unwrap(),
            change
        );
    }

    #[test]
    fn an_empty_keyword_set_is_a_real_change() {
        // Clearing the last keyword — marking a read message unread — is the case a
        // "skip it if there is nothing to write" shortcut would silently drop.
        let change =
            MailKeywordChange::new(ProviderKey::new("inbox/1/42").unwrap(), BTreeSet::new());
        assert!(change.keywords.is_empty());
    }
}
