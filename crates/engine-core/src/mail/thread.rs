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
