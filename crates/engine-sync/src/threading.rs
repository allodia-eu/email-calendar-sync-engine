//! Local thread derivation.
//!
//! Providers that assign their own thread ids (JMAP `Thread.id`, Gmail `threadId`,
//! Graph `conversationId`) set [`Message::thread_id`] during sync. Providers that do
//! not — notably IMAP — leave it `None`, so the engine **derives** it from the RFC
//! 5322 `Message-ID` / `In-Reply-To` / `References` headers (`modeling.md`: those are
//! threading hints, not identity).
//!
//! Derivation is **account-wide and cross-folder**: a reply filed in Sent and its
//! original in the Inbox are distinct provider objects in distinct scopes, but they
//! share message-ids, so they belong to one conversation (the Outlook/Gmail
//! behavior). It therefore runs as a post-sync pass over all the account's stored
//! messages, not inside a single scope's [`derive`](crate::ScopeSyncer) step.
//!
//! The grouping is a union-find over the message-id graph: two messages are united if
//! they share any id they own or reference (so a duplicate of one message in two
//! folders, and a reply that references its parent, both unite). Each component gets a
//! stable [`ThreadId`] (the lexicographically smallest owned `Message-ID`, falling
//! back to the smallest provider key). Subject-based linking is deliberately omitted
//! for now — it over-merges unrelated mail; the header graph is the safe baseline.
//!
//! Only messages without a provider-assigned thread id are touched, so running this
//! against a JMAP account is a no-op.

use core::time::Duration;
use std::collections::{BTreeSet, HashMap};

use engine_core::ids::{AccountId, MessageIdHeader, ProviderKey, ThreadId};
use engine_core::mail::Message;
use engine_core::sync::{ObjectKind, SyncScope, SyncUpdate};
use engine_store::{ApplyBatch, LeaseRequest, Store, StoreRead, WorkerId};

use crate::{SyncError, derive_messages};

/// What one [`derive_mail_threads`] pass changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadDeriveReport {
    /// Messages that gained a derived thread id.
    pub messages_assigned: usize,
    /// Distinct derived threads spanning those messages.
    pub threads: usize,
}

/// Derives and persists thread ids for one account's mail that lacks a
/// provider-assigned one, grouping messages across all the account's mail scopes
/// (folders) by their shared `Message-ID`/`In-Reply-To`/`References` headers.
///
/// Reads every mail scope's messages, computes the account-wide grouping, then
/// re-applies the changed messages per scope with their re-projected index rows —
/// **without advancing the scope cursor** (it is a derivation, not a sync), so the
/// next real sync still resumes from where it left off. Lease-gated like sync.
///
/// Idempotent: a message that already carries the derived id is re-applied to the same
/// value. Run it after [`sync_mail`](crate::sync_mail) completes.
///
/// # Errors
///
/// Returns [`SyncError`] if a scope read, claim, or apply fails (a live competing
/// lease surfaces as the retryable [`StoreError::ScopeHeld`](engine_store::StoreError)).
pub async fn derive_mail_threads<S>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<ThreadDeriveReport, SyncError>
where
    S: Store + StoreRead,
{
    // Gather every mail scope's live messages (cross-folder is the point).
    let scopes: Vec<SyncScope> = store
        .account_scopes(account.clone())
        .await?
        .into_iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Message))
        .collect();
    let mut per_scope: Vec<(SyncScope, Vec<Message>)> = Vec::with_capacity(scopes.len());
    let mut all: Vec<Message> = Vec::new();
    for scope in scopes {
        let mut messages = Vec::new();
        for (_key, payload) in store.scope_objects(&scope).await? {
            if let Ok(message) = serde_json::from_value::<Message>(payload) {
                all.push(message.clone());
                messages.push(message);
            }
        }
        per_scope.push((scope, messages));
    }

    let assignments = derive_thread_assignments(&all);
    let threads = assignments
        .values()
        .cloned()
        .collect::<BTreeSet<ThreadId>>()
        .len();

    // Persist per scope: re-apply only the messages that gained an id, re-projecting
    // their derived rows, leaving the cursor untouched.
    let mut messages_assigned = 0usize;
    for (scope, messages) in per_scope {
        let updated: Vec<Message> = messages
            .into_iter()
            .filter(|message| message.thread_id.is_none())
            .filter_map(|mut message| {
                let thread_id = assignments.get(message.id.key())?.clone();
                message.thread_id = Some(thread_id);
                Some(message)
            })
            .collect();
        if updated.is_empty() {
            continue;
        }
        messages_assigned += updated.len();

        let claim = store
            .claim_sync_scope(
                account.clone(),
                &scope,
                LeaseRequest::new(worker.clone(), ttl),
            )
            .await?;
        let update = SyncUpdate::delta(updated.clone(), Vec::new());
        let derived = derive_messages(&updated);
        let batch = ApplyBatch::with_cursor(&update, &derived, &[], None);
        match store.apply_sync_update(&claim.lease, batch).await {
            Ok(_) => store.release_sync_scope(claim.lease).await?,
            Err(err) => {
                let _ = store.release_sync_scope(claim.lease).await;
                return Err(err.into());
            }
        }
    }

    Ok(ThreadDeriveReport {
        messages_assigned,
        threads,
    })
}

/// Assigns a derived [`ThreadId`] to each message lacking a provider-assigned one,
/// grouping by the shared `Message-ID`/`In-Reply-To`/`References` graph. Returns the
/// derivable messages' provider keys mapped to their thread id; provider-threaded
/// messages are left out (their id stands). Pure — the unit of test for the grouping.
#[must_use]
pub(crate) fn derive_thread_assignments(messages: &[Message]) -> HashMap<ProviderKey, ThreadId> {
    let derivable: Vec<&Message> = messages
        .iter()
        .filter(|message| message.thread_id.is_none())
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
fn touched_ids(message: &Message) -> impl Iterator<Item = &str> {
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
    use super::*;
    use engine_core::ids::{MailboxId, MessageId};
    use engine_core::membership::Memberships;

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

        let assignments = derive_thread_assignments(&[original, reply, unrelated]);

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

        let assignments = derive_thread_assignments(&[inbox, archive]);
        assert_eq!(
            assignments[&ProviderKey::new("inbox-1").unwrap()],
            assignments[&ProviderKey::new("archive-1").unwrap()]
        );
    }

    #[test]
    fn provider_threaded_messages_are_left_untouched() {
        let mut native = message("jmap-1", "inbox", &["a@h"], &[]);
        native.thread_id = Some(ThreadId::try_from("T-provider").unwrap());

        let assignments = derive_thread_assignments(&[native]);
        // It already has a thread id, so derivation does not reassign it.
        assert!(assignments.is_empty());
    }

    #[test]
    fn a_message_with_no_headers_still_gets_a_singleton_thread() {
        // No Message-ID at all: the provider key is the stable fallback id.
        let bare = message("bare-1", "inbox", &[], &[]);
        let assignments = derive_thread_assignments(&[bare]);
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
        let assignments = derive_thread_assignments(&[m1, m2, m3]);
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
        let assignments = derive_thread_assignments(&[first, second]);
        assert_eq!(
            assignments[&ProviderKey::new("k1").unwrap()].as_str(),
            "a@h"
        );
    }
}
