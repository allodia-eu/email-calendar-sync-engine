//! The pure grouping: the message-id graph, and the id a component is named after.
//!
//! Split from [`super`] — which reads the store, claims leases and writes — because this half is
//! I/O-free and is the unit the grouping rules are tested through.

use std::collections::HashMap;

use engine_core::{
    ids::{MessageIdHeader, ProviderKey, ThreadId},
    mail::{StoredContent, ThreadRef},
};

/// Assigns a derived [`ThreadId`] to each message lacking a provider-assigned one,
/// grouping by the shared `Message-ID`/`In-Reply-To`/`References` graph. Returns the
/// derivable messages' provider keys mapped to their thread id; provider-threaded
/// messages are left out (their id stands). Pure — the unit of test for the grouping.
///
/// A message the engine already threaded is derivable *again*: the assignment is a
/// function of the whole input, not of what each message happens to carry, so a reply
/// synced after its thread was derived still lands in it. Feeding provider-threaded
/// messages in instead would let a `References` header merge threads the provider
/// separated.
#[must_use]
pub(crate) fn derive_thread_assignments(
    messages: &[StoredContent],
) -> HashMap<ProviderKey, ThreadId> {
    let derivable: Vec<&StoredContent> = messages
        .iter()
        .filter(|message| message.thread.as_ref().is_none_or(ThreadRef::is_derived))
        .collect();
    let mut groups = UnionFind::new(derivable.len());

    // Unite any two messages that touch a common id (owned or referenced): a reply
    // references its parent's id; a duplicate shares its own id.
    let mut rep: HashMap<&str, usize> = HashMap::new();
    for (index, message) in derivable.iter().enumerate() {
        for id in touched_ids(message) {
            match rep.get(id) {
                Some(&seen) => groups.union(index, seen),
                None => {
                    rep.insert(id, index);
                }
            }
        }
    }

    // A stable id per component: the smallest owned Message-ID, else the smallest key.
    let mut owned_min: HashMap<usize, &str> = HashMap::new();
    let mut key_min: HashMap<usize, &str> = HashMap::new();
    for (index, message) in derivable.iter().enumerate() {
        let root = groups.find(index);
        for header in &message.envelope.message_id {
            owned_min
                .entry(root)
                .and_modify(|current| {
                    if header.as_str() < *current {
                        *current = header.as_str();
                    }
                })
                .or_insert_with(|| header.as_str());
        }
        let key = message.id.key().as_str();
        key_min
            .entry(root)
            .and_modify(|current| {
                if key < *current {
                    *current = key;
                }
            })
            .or_insert(key);
    }

    let mut assignments = HashMap::new();
    for (index, message) in derivable.iter().enumerate() {
        let root = groups.find(index);
        let thread_id = owned_min
            .get(&root)
            .or_else(|| key_min.get(&root))
            .copied()
            .and_then(|id| ThreadId::try_from(id).ok());
        if let Some(thread_id) = thread_id {
            assignments.insert(message.id.key().clone(), thread_id);
        }
    }
    assignments
}

/// Every `Message-ID`/`In-Reply-To`/`References` value the message touches.
fn touched_ids(message: &StoredContent) -> impl Iterator<Item = &str> {
    message
        .envelope
        .message_id
        .iter()
        .chain(message.envelope.in_reply_to.iter())
        .chain(message.envelope.references.iter())
        .map(MessageIdHeader::as_str)
}

/// A minimal disjoint-set over message indices, with path compression.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut node = index;
        while self.parent[node] != root {
            let next = self.parent[node];
            self.parent[node] = root;
            node = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{MailboxId, MessageId},
        mail::{MailContent, Message},
        membership::Memberships,
    };

    use super::*;

    /// The stored payload of a message, decoded — the round trip storage performs, so a field
    /// that stops being serialized stops reaching the threading pass here too.
    fn stored(message: &Message) -> StoredContent {
        serde_json::from_value(serde_json::to_value(MailContent::from(message)).unwrap()).unwrap()
    }

    /// Builds a message with the given owned id and referenced ids, in a mailbox.
    fn message(id: &str, mailbox: &str, owned: &[&str], references: &[&str]) -> Message {
        let mut message = Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
        );
        message.envelope.message_id = owned
            .iter()
            .map(|s| MessageIdHeader::new(*s).unwrap())
            .collect();
        message.envelope.references = references
            .iter()
            .map(|s| MessageIdHeader::new(*s).unwrap())
            .collect();
        message
    }

    #[test]
    fn reply_threads_with_its_parent_across_folders() {
        // The original in "inbox" and the reply in "sent" (a distinct object/scope)
        // share an id via References, so they land in one thread. Ids are chosen so the
        // original's is lexicographically smallest (the stable thread id).
        let original = message("inbox-1", "inbox", &["a-orig@h"], &[]);
        let reply = message("sent-1", "sent", &["b-reply@h"], &["a-orig@h"]);
        let unrelated = message("inbox-2", "inbox", &["c-other@h"], &[]);

        let assignments =
            derive_thread_assignments(&[stored(&original), stored(&reply), stored(&unrelated)]);

        let inbox1 = ProviderKey::new("inbox-1").unwrap();
        let sent1 = ProviderKey::new("sent-1").unwrap();
        let inbox2 = ProviderKey::new("inbox-2").unwrap();
        assert_eq!(assignments[&inbox1], assignments[&sent1]);
        assert_ne!(assignments[&inbox1], assignments[&inbox2]);
        // The thread id is the smallest owned Message-ID in the component.
        assert_eq!(assignments[&inbox1].as_str(), "a-orig@h");
    }

    #[test]
    fn duplicate_message_id_across_folders_is_one_thread() {
        // The same RFC 5322 message copied into two folders (distinct provider keys,
        // same Message-ID) is a single conversation.
        let inbox = message("inbox-1", "inbox", &["dup@h"], &[]);
        let archive = message("archive-1", "archive", &["dup@h"], &[]);

        let assignments = derive_thread_assignments(&[stored(&inbox), stored(&archive)]);
        assert_eq!(
            assignments[&ProviderKey::new("inbox-1").unwrap()],
            assignments[&ProviderKey::new("archive-1").unwrap()]
        );
    }

    #[test]
    fn provider_threaded_messages_are_left_untouched() {
        let mut native = message("jmap-1", "inbox", &["a@h"], &[]);
        native.thread = Some(ThreadRef::provider_assigned(
            ThreadId::try_from("T-provider").unwrap(),
        ));

        let assignments = derive_thread_assignments(&[stored(&native)]);
        // It already has a thread id, so derivation does not reassign it.
        assert!(assignments.is_empty());
    }

    #[test]
    fn a_reply_synced_after_its_thread_was_derived_still_joins_it() {
        // The bug this pass regressed on: pass one derives a thread over the mail then
        // in the store; the reply arrives in a later sync, alone. It must unite with the
        // thread it references, not become a singleton — so an already-derived message
        // has to re-enter the graph.
        let mut original = message("k1", "inbox", &["a@h"], &[]);
        let first = derive_thread_assignments(&[stored(&original)]);
        let derived = first[&ProviderKey::new("k1").unwrap()].clone();
        assert_eq!(derived.as_str(), "a@h");
        original.thread = Some(ThreadRef::derived(derived.clone()));

        let reply = message("k2", "inbox", &["b@h"], &["a@h"]);
        let second = derive_thread_assignments(&[stored(&original), stored(&reply)]);
        assert_eq!(second[&ProviderKey::new("k1").unwrap()], derived);
        assert_eq!(second[&ProviderKey::new("k2").unwrap()], derived);
    }

    #[test]
    fn a_late_message_owning_a_smaller_id_rekeys_the_thread() {
        // The component id is a function of the component, so a joining message that owns
        // a smaller Message-ID re-keys the whole thread — including the incumbent, which
        // must be re-derived (and re-applied) to the new id.
        let mut incumbent = message("k1", "inbox", &["z@h"], &[]);
        incumbent.thread = Some(ThreadRef::derived(ThreadId::try_from("z@h").unwrap()));
        let joining = message("k2", "inbox", &["a@h"], &["z@h"]);

        let assignments = derive_thread_assignments(&[stored(&incumbent), stored(&joining)]);
        assert_eq!(
            assignments[&ProviderKey::new("k1").unwrap()].as_str(),
            "a@h"
        );
        assert_eq!(
            assignments[&ProviderKey::new("k2").unwrap()].as_str(),
            "a@h"
        );
    }

    #[test]
    fn a_derivable_message_never_merges_into_a_provider_threaded_one() {
        // A reply referencing a provider-threaded message must not pull that thread into
        // the derived graph: the provider's grouping is authoritative, and a forged
        // References header could otherwise merge two threads it kept apart.
        let mut native = message("jmap-1", "inbox", &["a@h"], &[]);
        native.thread = Some(ThreadRef::provider_assigned(
            ThreadId::try_from("T-provider").unwrap(),
        ));
        let reply = message("imap-1", "inbox", &["b@h"], &["a@h"]);

        let assignments = derive_thread_assignments(&[stored(&native), stored(&reply)]);
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[&ProviderKey::new("imap-1").unwrap()].as_str(),
            "b@h"
        );
    }

    #[test]
    fn a_message_with_no_headers_still_gets_a_singleton_thread() {
        // No Message-ID at all: the provider key is the stable fallback id.
        let bare = message("bare-1", "inbox", &[], &[]);
        let assignments = derive_thread_assignments(&[stored(&bare)]);
        assert_eq!(
            assignments[&ProviderKey::new("bare-1").unwrap()].as_str(),
            "bare-1"
        );
    }

    #[test]
    fn a_message_referencing_two_threads_merges_them() {
        // m3 references two previously-distinct messages, uniting all three into one
        // thread (and exercising union-find path compression across the merge).
        let m1 = message("k1", "inbox", &["a@h"], &[]);
        let m2 = message("k2", "inbox", &["b@h"], &[]);
        let m3 = message("k3", "inbox", &["c@h"], &["a@h", "b@h"]);
        let assignments = derive_thread_assignments(&[stored(&m1), stored(&m2), stored(&m3)]);
        let t1 = assignments[&ProviderKey::new("k1").unwrap()].clone();
        assert_eq!(assignments[&ProviderKey::new("k2").unwrap()], t1);
        assert_eq!(assignments[&ProviderKey::new("k3").unwrap()], t1);
        assert_eq!(t1.as_str(), "a@h");
    }

    #[test]
    fn thread_id_is_the_smallest_owned_id_regardless_of_arrival_order() {
        // The first-seen message owns the larger id; the second (same thread, via a
        // reference) owns the smaller — the smaller wins, independent of order.
        let first = message("k1", "inbox", &["z@h"], &[]);
        let second = message("k2", "inbox", &["a@h"], &["z@h"]);
        let assignments = derive_thread_assignments(&[stored(&first), stored(&second)]);
        assert_eq!(
            assignments[&ProviderKey::new("k1").unwrap()].as_str(),
            "a@h"
        );
    }
}
