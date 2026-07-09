//! Threads.

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, ThreadId};

/// Where a thread id came from.
///
/// A late-arriving message can connect two previously separate locally-derived
/// threads; provider-assigned threads change only when the provider says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ThreadProvenance {
    /// The thread id was assigned by the provider (JMAP `Thread.id`, Gmail
    /// `threadId`, Graph `conversationId`).
    ProviderAssigned,
    /// The thread id was derived locally from `Message-ID`/`References`/subject
    /// when the provider exposes no threading.
    LocallyDerived,
}

/// The thread a [`Message`](crate::mail::Message) belongs to: the id, plus where
/// that id came from.
///
/// The provenance is load-bearing, not decoration. Local derivation re-runs after
/// every sync and must re-group the mail it grouped before — a reply that arrives
/// later joins (and can re-key) the thread it belongs to. It can only do that if it
/// can tell its own [`LocallyDerived`](ThreadProvenance::LocallyDerived) ids apart
/// from [`ProviderAssigned`](ThreadProvenance::ProviderAssigned) ones, which it must
/// never touch — a stray `References` header would otherwise merge two threads the
/// provider deliberately kept apart (`threading.md`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadRef {
    /// The thread's id.
    pub id: ThreadId,
    /// Whether the id is provider-assigned or locally derived.
    pub provenance: ThreadProvenance,
}

impl ThreadRef {
    /// A thread id the provider assigned (JMAP `Thread.id`, Gmail `threadId`, Graph
    /// `conversationId`).
    #[must_use]
    pub fn provider_assigned(id: ThreadId) -> Self {
        Self {
            id,
            provenance: ThreadProvenance::ProviderAssigned,
        }
    }

    /// A thread id the engine derived from the `Message-ID`/`References` graph.
    #[must_use]
    pub fn derived(id: ThreadId) -> Self {
        Self {
            id,
            provenance: ThreadProvenance::LocallyDerived,
        }
    }

    /// Returns `true` if the id was derived locally, and so may be re-grouped.
    #[must_use]
    pub fn is_derived(&self) -> bool {
        self.provenance == ThreadProvenance::LocallyDerived
    }
}

/// A thread: an ordered set of messages that belong together.
///
/// `message_ids` is ordered oldest-first by received time (RFC 8621 §3). Every
/// message belongs to exactly one thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// The thread's id.
    pub id: ThreadId,
    /// Whether the id is provider-assigned or locally derived.
    pub provenance: ThreadProvenance,
    /// The member messages, oldest-first.
    pub message_ids: Vec<MessageId>,
}

impl Thread {
    /// Creates a thread from its id, provenance, and ordered members.
    #[must_use]
    pub fn new(id: ThreadId, provenance: ThreadProvenance, message_ids: Vec<MessageId>) -> Self {
        Self {
            id,
            provenance,
            message_ids,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_ref_records_where_its_id_came_from() {
        let derived = ThreadRef::derived(ThreadId::try_from("a@h").unwrap());
        assert!(derived.is_derived());
        assert!(!ThreadRef::provider_assigned(ThreadId::try_from("T1").unwrap()).is_derived());

        let json = serde_json::to_string(&derived).unwrap();
        assert_eq!(serde_json::from_str::<ThreadRef>(&json).unwrap(), derived);
    }

    #[test]
    fn thread_records_provenance_and_order() {
        let thread = Thread::new(
            ThreadId::try_from("t1").unwrap(),
            ThreadProvenance::LocallyDerived,
            vec![
                MessageId::try_from("m1").unwrap(),
                MessageId::try_from("m2").unwrap(),
            ],
        );
        assert_eq!(thread.provenance, ThreadProvenance::LocallyDerived);
        assert_eq!(thread.message_ids.len(), 2);
        let json = serde_json::to_string(&thread).unwrap();
        assert_eq!(serde_json::from_str::<Thread>(&json).unwrap(), thread);
    }
}
