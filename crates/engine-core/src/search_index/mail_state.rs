//! The rows a *partial* mail write touches: a state-only change, and a thread assignment.
//!
//! Split from [`super::mail`], which projects a whole object. These two name what moved and
//! nothing else, so a store writes exactly that — the distinction the `message` table's three
//! write shapes rest on.

use serde::{Deserialize, Serialize};

use crate::{
    ids::{ProviderKey, ThreadId},
    mail::{MailFlags, MailStateChange},
    time::UtcDateTime,
    version::RevisionTokens,
};

/// The rows a [`MailStateChange`] rewrites: the `message` row's state columns, and the
/// message's `keyword`-kind memberships.
///
/// Deliberately not a [`MailRow`]: a state change carries no subject, no sender and no date, so
/// a whole-row upsert built from one would blank every column the provider did not send. This
/// names what moved, and a store writes exactly that.
///
/// A new state axis becomes a field here and a `membership` kind beside `keywords` — Graph's
/// `categories` is the next one — so the store gains an axis without gaining a mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailStateRow {
    /// The message whose state moved.
    pub key: ProviderKey,
    /// The system keywords, as the bitfield the `message` row sorts and filters on.
    pub flags: MailFlags,
    /// The complete keyword set, as the membership values `keyword:` searches. Replaces the
    /// message's existing keyword memberships.
    pub keywords: Vec<String>,
    /// The complete set of mailboxes the message is filed in, when the provider files **in
    /// place** — `None` when it files through identity and the mailbox memberships are not this
    /// change's to touch. See [`MailState::mailboxes`](crate::mail::MailState::mailboxes).
    pub mailboxes: Option<Vec<String>>,
    /// The revision tokens a conditional write quotes. State, not content: they bump when the
    /// message's state moves, so a copy in the payload would go stale on a mark-read.
    pub revisions: RevisionTokens,
    /// When the provider last changed the object.
    pub last_modified: Option<UtcDateTime>,
}

/// The row a thread assignment rewrites: the `message` row's `thread_id` column, alone.
///
/// The engine derives a thread id from the reference graph, so it is the engine's to write and
/// no provider's to send. Writing it as a whole-row upsert — re-projected from a stored payload
/// — would carry every *other* column along with it, including the flags a keyword change had
/// just moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailThreadRow {
    /// The message being assigned.
    pub key: ProviderKey,
    /// The thread it now belongs to.
    pub thread_id: ThreadId,
}

/// Projects a [`MailStateChange`] into the rows a state-only write touches.
#[must_use]
pub fn project_state_change(change: &MailStateChange) -> MailStateRow {
    MailStateRow {
        key: change.key.clone(),
        flags: MailFlags::from_keywords(&change.state.keywords),
        revisions: change.state.revisions.clone(),
        last_modified: change.state.last_modified,
        keywords: change
            .state
            .keywords
            .iter()
            .map(|keyword| keyword.as_str().to_owned())
            .collect(),
        mailboxes: change.state.mailboxes.as_ref().map(|mailboxes| {
            mailboxes
                .iter()
                .map(|mailbox| mailbox.as_str().to_owned())
                .collect()
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{Keyword, SystemKeyword};

    #[test]
    fn state_change_projects_the_bitfield_and_the_membership_values() {
        let change = MailStateChange::keywords(
            ProviderKey::new("m1").unwrap(),
            [
                Keyword::system(SystemKeyword::Seen),
                Keyword::new("todo").unwrap(),
            ]
            .into_iter()
            .collect(),
        );
        let row = project_state_change(&change);
        assert_eq!(row.key.as_str(), "m1");
        assert!(row.flags.seen());
        assert!(!row.flags.flagged());
        // The user keyword reaches the membership values but not the bitfield, which
        // carries only the system keywords a list row's appearance depends on.
        assert_eq!(row.keywords, vec!["$seen".to_owned(), "todo".to_owned()]);
    }

    #[test]
    fn clearing_every_keyword_projects_an_empty_row_rather_than_nothing() {
        // Marking a read message unread empties the set. The row must still be produced —
        // a store that skipped it would leave the message `$seen` forever.
        let row = project_state_change(&MailStateChange::keywords(
            ProviderKey::new("m1").unwrap(),
            std::collections::BTreeSet::new(),
        ));
        assert_eq!(row.flags.bits(), 0);
        assert!(row.keywords.is_empty());
    }
}
