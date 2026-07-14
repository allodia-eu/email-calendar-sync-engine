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
    ids::{AccountId, ProviderKey},
    search_index::{
        EventIndexRow, EventParticipantRow, MailAddressRow, MailIndexRow, MembershipRow,
    },
    sync::{SyncScope, SyncState},
    time::{ExpansionWindow, UtcDateTime},
    write::{IdempotencyKey, PendingOp, PendingOpId},
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    apply::{DerivedWrite, FtsField, OccurrenceRow, StorableObject},
    error::{Result, StoreError},
    lease::{Clock, FenceToken, LeaseRequest},
    outbox::PendingOpState,
};

mod read;
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
struct ScopeCell {
    token: FenceToken,
    lease_expiry: Option<UtcDateTime>,
    state: Option<SyncState>,
    /// The window this scope's occurrence rows are materialized over (event scopes only).
    window: Option<ExpansionWindow>,
    objects: HashMap<ProviderKey, Value>,
    fts: HashMap<ProviderKey, Vec<FtsField>>,
    occurrences: HashMap<ProviderKey, Vec<OccurrenceRow>>,
    mail_index: HashMap<ProviderKey, MailIndexRow>,
    addresses: HashMap<ProviderKey, Vec<MailAddressRow>>,
    memberships: HashMap<ProviderKey, Vec<MembershipRow>>,
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
            mail_index: HashMap::new(),
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
        self.mail_index.remove(key);
        self.addresses.remove(key);
        self.memberships.remove(key);
        self.event_index.remove(key);
        self.participants.remove(key);
    }

    /// Serializes and upserts an object's normalized payload, keyed by its
    /// provider key.
    fn upsert_object<T: StorableObject + Serialize>(&mut self, obj: &T) -> Result<()> {
        let value = serde_json::to_value(obj).map_err(|e| StoreError::Backend(e.to_string()))?;
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
        for row in &derived.mail_index {
            self.mail_index.insert(row.key.clone(), row.clone());
        }
        for row in &derived.event_index {
            self.event_index.insert(row.key.clone(), row.clone());
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
