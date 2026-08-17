//! The message-id graph: the rows threading is maintained from.
//!
//! A conversation is a connected component of the ids messages own (`Message-ID`) and reference
//! (`In-Reply-To`, `References`). Storing that graph as rows is what turns thread derivation from
//! a scan of every payload in the account into an indexed lookup of the components one incoming
//! message touches (`threading.md`).
//!
//! **Only derivable messages are in it.** A provider that assigns its own thread ids is
//! authoritative, and a forged `References` header must not merge two threads it kept apart — so
//! a provider-threaded message projects no rows at all and the graph *is* the derivable set, by
//! construction rather than by a filter every reader has to remember.

use serde::{Deserialize, Serialize};

use crate::{
    ids::{MessageIdHeader, ProviderKey},
    mail::{Envelope, ThreadRef},
};

/// One id a message owns or references (the `msgid_ref` table).
///
/// `owned` separates the two because they answer different questions: any id — owned or
/// referenced — *joins* a message to a component, but only an owned one can *name* the resulting
/// thread. Two replies that both reference a root nobody has yet still belong together, and their
/// thread is named after one of them, not after the absent root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailRefRow {
    /// The message the id belongs to.
    pub key: ProviderKey,
    /// The id itself, as the header spelled it.
    pub msgid: MessageIdHeader,
    /// Whether the message **owns** this id (its own `Message-ID`) rather than merely
    /// referencing it.
    pub owned: bool,
}

/// Projects one message's `Message-ID`/`In-Reply-To`/`References` ids into graph rows.
///
/// Returns nothing for a **provider-threaded** message: its thread is the provider's to decide,
/// so it is not in the graph and nothing in the graph can reach it.
///
/// Takes the parts rather than a `Message` so the same projection serves a stored payload, which
/// is what a rebuild and the schema backfill read. A message that both owns and references an id
/// yields one row for it, owned.
#[must_use]
pub fn project_refs(
    key: &ProviderKey,
    envelope: &Envelope,
    thread: Option<&ThreadRef>,
) -> Vec<MailRefRow> {
    if thread.is_some_and(|thread| !thread.is_derived()) {
        return Vec::new();
    }
    let mut rows: Vec<MailRefRow> = Vec::new();
    let mut push = |msgid: &MessageIdHeader, owned: bool| {
        match rows.iter_mut().find(|row| &row.msgid == msgid) {
            // Owned wins: an id a message both owns and references names its thread.
            Some(existing) => existing.owned |= owned,
            None => rows.push(MailRefRow {
                key: key.clone(),
                msgid: msgid.clone(),
                owned,
            }),
        }
    };
    for msgid in &envelope.message_id {
        push(msgid, true);
    }
    for msgid in envelope.in_reply_to.iter().chain(&envelope.references) {
        push(msgid, false);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ThreadId;

    fn envelope(owned: &[&str], referenced: &[&str]) -> Envelope {
        Envelope {
            message_id: owned
                .iter()
                .map(|id| MessageIdHeader::new(*id).unwrap())
                .collect(),
            references: referenced
                .iter()
                .map(|id| MessageIdHeader::new(*id).unwrap())
                .collect(),
            ..Envelope::default()
        }
    }

    fn key() -> ProviderKey {
        ProviderKey::new("m1").unwrap()
    }

    #[test]
    fn an_owned_id_and_a_referenced_one_are_told_apart() {
        let rows = project_refs(&key(), &envelope(&["b@h"], &["a@h"]), None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.msgid.as_str() == "b@h" && r.owned));
        assert!(rows.iter().any(|r| r.msgid.as_str() == "a@h" && !r.owned));
    }

    #[test]
    fn an_id_both_owned_and_referenced_is_one_owned_row() {
        // A message quoting its own id in References must still be able to name its thread.
        let rows = project_refs(&key(), &envelope(&["a@h"], &["a@h"]), None);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].owned);
    }

    #[test]
    fn a_provider_threaded_message_is_not_in_the_graph() {
        let thread = ThreadRef::provider_assigned(ThreadId::try_from("T-1").unwrap());
        assert!(
            project_refs(&key(), &envelope(&["a@h"], &["b@h"]), Some(&thread)).is_empty(),
            "a forged References header must not reach a thread the provider decided"
        );
    }

    #[test]
    fn a_message_the_engine_already_threaded_stays_in_the_graph() {
        // Its component can still grow: a reply synced later has to be able to find it.
        let thread = ThreadRef::derived(ThreadId::try_from("a@h").unwrap());
        assert_eq!(
            project_refs(&key(), &envelope(&["a@h"], &[]), Some(&thread)).len(),
            1
        );
    }

    #[test]
    fn a_message_with_no_headers_has_no_rows() {
        assert!(project_refs(&key(), &envelope(&[], &[]), None).is_empty());
    }
}
