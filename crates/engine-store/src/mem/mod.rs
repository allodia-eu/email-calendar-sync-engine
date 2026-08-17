//! An in-memory reference [`Store`](crate::Store).
//!
//! This is the executable specification of the concurrency contract: it enforces
//! fencing tokens, atomic per-scope apply, snapshot tombstoning, derived-row
//! commit/tombstone, and the outbox state machine. The reusable [`contract`]
//! suite runs against it, and every real backend (`store-sqlite`, a future
//! `store-postgres`) must satisfy the same suite. It is also a useful test double
//! for `engine-sync` before a persistent store exists.
//!
//! Liveness is tracked by lease *expiry*; the fencing *token* is the actual
//! serialization mechanism (an older token is rejected even before its lease
//! expires once a newer claim bumps the generation).
//!
//! [`contract`]: crate::contract

use core::fmt;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

use engine_core::{
    ids::{AccountId, ContactId, MessageId, ProviderKey},
    people::{CanonicalEmail, PeopleSnapshot},
    recipient::{RecipientCoverage, RecipientObservation},
    search_index::{
        EventIndexRow, EventParticipantRow, MailAddressRow, MailRefRow, MailRow, MembershipKind,
        MembershipRow,
    },
    sync::{SyncObject, SyncScope, SyncState},
    time::{ExpansionWindow, UtcDateTime},
    write::{IdempotencyKey, PendingOp, PendingOpId},
};
use serde_json::Value;

use crate::{
    CachedContactPhoto, ContactSourceAvailability,
    apply::{DerivedWrite, FtsField, OccurrenceRow},
    error::{Result, StoreError},
    lease::{Clock, FenceToken, LeaseRequest},
    outbox::PendingOpState,
};

mod contact;
mod read;
mod threading;
mod write;

#[cfg(test)]
mod tests;

/// Returns `true` if a lease is held and has not expired at `now`.
fn is_live(expiry: Option<UtcDateTime>, now: UtcDateTime) -> bool {
    expiry.is_some_and(|e| e > now)
}

/// Groups flat junction rows by their object key, so each object's rows can
/// *replace* (not append to) the stored set — the idempotent-on-replay semantics
/// the structured index requires (`store-and-sync.md`).
fn group_by_key<R: Clone>(
    rows: &[R],
    key_of: impl Fn(&R) -> &ProviderKey,
) -> HashMap<ProviderKey, Vec<R>> {
    let mut grouped: HashMap<ProviderKey, Vec<R>> = HashMap::new();
    for row in rows {
        grouped
            .entry(key_of(row).clone())
            .or_default()
            .push(row.clone());
    }
    grouped
}

/// Per-scope state: the fencing generation, lease expiry, cursor, objects, and
/// derived rows.
pub(super) struct ScopeCell {
    token: FenceToken,
    lease_expiry: Option<UtcDateTime>,
    state: Option<SyncState>,
    /// The window this scope's occurrence rows are materialized over (event scopes only).
    window: Option<ExpansionWindow>,
    objects: HashMap<ProviderKey, Value>,
    fts: HashMap<ProviderKey, Vec<FtsField>>,
    occurrences: HashMap<ProviderKey, Vec<OccurrenceRow>>,
    pub(super) messages: HashMap<ProviderKey, MailRow>,
    /// The message-id graph rows, per message — empty for a provider-threaded one, which is not
    /// in the graph at all.
    pub(super) refs: HashMap<ProviderKey, Vec<MailRefRow>>,
    addresses: HashMap<ProviderKey, Vec<MailAddressRow>>,
    pub(super) memberships: HashMap<ProviderKey, Vec<MembershipRow>>,
    event_index: HashMap<ProviderKey, EventIndexRow>,
    participants: HashMap<ProviderKey, Vec<EventParticipantRow>>,
}

impl ScopeCell {
    fn new() -> Self {
        Self {
            token: FenceToken::initial(),
            lease_expiry: None,
            state: None,
            window: None,
            objects: HashMap::new(),
            fts: HashMap::new(),
            occurrences: HashMap::new(),
            messages: HashMap::new(),
            refs: HashMap::new(),
            addresses: HashMap::new(),
            memberships: HashMap::new(),
            event_index: HashMap::new(),
            participants: HashMap::new(),
        }
    }

    /// Removes an object and any derived rows keyed by it. Returns whether the
    /// object existed.
    fn tombstone(&mut self, key: &ProviderKey) -> bool {
        let existed = self.objects.remove(key).is_some();
        self.remove_derived(key);
        existed
    }

    /// Removes every derived row kind for one key (tombstone and explicit
    /// `removed` share this).
    fn remove_derived(&mut self, key: &ProviderKey) {
        self.fts.remove(key);
        self.occurrences.remove(key);
        self.messages.remove(key);
        self.refs.remove(key);
        self.addresses.remove(key);
        self.memberships.remove(key);
        self.event_index.remove(key);
        self.participants.remove(key);
    }

    /// Serializes and upserts an object's normalized payload, keyed by its
    /// provider key.
    fn upsert_object<T: SyncObject>(&mut self, obj: &T) -> Result<()> {
        // Matches `store-sqlite`: the object decides its stored record, so the reference store
        // cannot hold a field the real one drops.
        let value = obj
            .to_payload()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        self.objects.insert(obj.provider_key().clone(), value);
        Ok(())
    }

    /// Applies precomputed derived rows (shared by apply and maintenance).
    ///
    /// `removed` is cleared **first**, then the upserts, so a single re-expansion
    /// batch (`{removed: [event], occurrences: [fresh]}`) clears the stale rows and
    /// writes the fresh ones in one pass without the clear wiping the new rows
    /// (matches `store-sqlite`). Full-text and structured rows *replace* per object
    /// (idempotent on replay); occurrences append (the store keys them by instant,
    /// so a real backend is idempotent — the reference store's append is the known
    /// divergence noted in `store-and-sync.md`).
    fn apply_derived(&mut self, derived: &DerivedWrite) {
        for key in &derived.removed {
            self.remove_derived(key);
        }
        for key in &derived.reset_occurrences {
            self.occurrences.remove(key);
        }
        for row in &derived.fts {
            self.fts.insert(row.key.clone(), row.fields.clone());
        }
        for occ in &derived.occurrences {
            self.occurrences
                .entry(occ.event.clone())
                .or_default()
                .push(occ.clone());
        }
        for row in &derived.messages {
            let mut row = row.clone();
            // `thread_id` and `preview` are the two columns no provider supplies: the first is
            // engine-derived wherever the provider assigns no thread ids, the second computed by
            // the body sync wherever there is no server snippet. `None` from a whole object means
            // "nothing to say", so what is stored is kept rather than blanked (matches
            // `store-sqlite`'s `COALESCE`).
            if let Some(existing) = self.messages.get(&row.key) {
                row.thread_id = row.thread_id.or_else(|| existing.thread_id.clone());
                row.preview = row.preview.or_else(|| existing.preview.clone());
            }
            self.messages.insert(row.key.clone(), row);
        }
        for row in &derived.state_changes {
            // An update, not an insert: a keyword change carries no subject, sender or date,
            // so a message the store has never seen gets no row from one — and, because the
            // junction would happily hold rows for a message that is not there, no membership
            // row either (matches `store-sqlite`, which gates on the `UPDATE`'s row count).
            let Some(existing) = self.messages.get_mut(&row.key) else {
                continue;
            };
            existing.flags = row.flags;
            // A partial names the tokens that moved and is silent about the rest, so what it
            // does not carry is kept rather than blanked (matches `store-sqlite`'s `COALESCE`).
            existing.revisions = row.revisions.clone().or(&existing.revisions);
            existing.last_modified = row.last_modified.or(existing.last_modified);
            // Each kind is replaced on its own. The keyword memberships always; the mailbox
            // ones only when the change carries filing — `None` means the provider files
            // through identity and has nothing to say about which folder this is in, so
            // clearing them would drop the message out of it (matches `store-sqlite`).
            let memberships = self.memberships.entry(row.key.clone()).or_default();
            memberships.retain(|m| m.kind != MembershipKind::Keyword);
            memberships.extend(row.keywords.iter().map(|value| MembershipRow {
                key: row.key.clone(),
                kind: MembershipKind::Keyword,
                value: value.clone(),
            }));
            if let Some(mailboxes) = &row.mailboxes {
                memberships.retain(|m| m.kind != MembershipKind::Mailbox);
                memberships.extend(mailboxes.iter().map(|value| MembershipRow {
                    key: row.key.clone(),
                    kind: MembershipKind::Mailbox,
                    value: value.clone(),
                }));
            }
        }
        for row in &derived.thread_assignments {
            if let Some(existing) = self.messages.get_mut(&row.key) {
                existing.thread_id = Some(row.thread_id.clone());
            }
        }
        for row in &derived.event_index {
            self.event_index.insert(row.key.clone(), row.clone());
        }
        for (key, rows) in group_by_key(&derived.msgid_refs, |r| &r.key) {
            self.refs.insert(key, rows);
        }
        for (key, rows) in group_by_key(&derived.addresses, |r| &r.key) {
            self.addresses.insert(key, rows);
        }
        for (key, rows) in group_by_key(&derived.memberships, |r| &r.key) {
            self.memberships.insert(key, rows);
        }
        for (key, rows) in group_by_key(&derived.participants, |r| &r.key) {
            self.participants.insert(key, rows);
        }
    }
}

/// Per-op outbox state.
struct OpCell {
    account: AccountId,
    op: PendingOp,
    state: PendingOpState,
    token: FenceToken,
    lease_expiry: Option<UtcDateTime>,
}

/// The whole store state, behind one mutex (a reference impl, not a throughput
/// target).
struct Inner {
    scopes: HashMap<SyncScope, ScopeCell>,
    ops: BTreeMap<PendingOpId, OpCell>,
    idempotency: HashMap<(AccountId, IdempotencyKey), PendingOpId>,
    next_op: u64,
    contact_generation: u64,
    people: PeopleSnapshot,
    observations: BTreeMap<(AccountId, MessageId, CanonicalEmail), ObservationCell>,
    recipient_versions: BTreeMap<AccountId, u32>,
    recipient_coverage: BTreeMap<AccountId, RecipientCoverage>,
    contact_availability: BTreeMap<SyncScope, ContactSourceAvailability>,
    /// Keyed by (account, contact, **resource**) — a card can carry several media
    /// resources and they must not share a cache entry.
    contact_photos: BTreeMap<(AccountId, ContactId, String), CachedContactPhoto>,
}

/// One durable observation row. Suppression survives an idempotent replay.
#[derive(Debug, Clone)]
struct ObservationCell {
    observation: RecipientObservation,
    suppressed: bool,
}

/// An in-memory [`Store`](crate::Store) + [`StoreRead`](crate::StoreRead),
/// parameterized by an injected [`Clock`] for lease-expiry control.
pub struct MemStore<C> {
    clock: C,
    inner: Mutex<Inner>,
}

impl<C> fmt::Debug for MemStore<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemStore").finish_non_exhaustive()
    }
}

impl<C: Clock> MemStore<C> {
    /// Creates an empty store driven by `clock`.
    #[must_use]
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            inner: Mutex::new(Inner {
                scopes: HashMap::new(),
                ops: BTreeMap::new(),
                idempotency: HashMap::new(),
                next_op: 0,
                contact_generation: 0,
                people: PeopleSnapshot::empty(),
                observations: BTreeMap::new(),
                recipient_versions: BTreeMap::new(),
                recipient_coverage: BTreeMap::new(),
                contact_availability: BTreeMap::new(),
                contact_photos: BTreeMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("store mutex poisoned")
    }
}

/// Computes a lease expiry from the current instant and a request's TTL.
fn expiry_after(now: UtcDateTime, req: &LeaseRequest) -> Result<UtcDateTime> {
    now.checked_add(req.ttl)
        .ok_or_else(|| StoreError::Backend("lease ttl overflow".to_owned()))
}
