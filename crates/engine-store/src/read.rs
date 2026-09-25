//! The lease-free read surface: [`StoreRead`] and the row shapes it returns.
//!
//! Enough for the contract suite to verify stored state, for early diagnostics, and
//! for a host to render what the store already holds. The writer/lease/outbox half is
//! [`Store`](crate::Store), in `store.rs`.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey, ThreadId},
    mail::Keyword,
    search_index::MailRow,
    sync::SyncScope,
    time::{ExpansionWindow, Horizon, UtcDateTime},
    write::PendingOpId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    apply::OccurrenceRow,
    error::Result,
    outbox::{PendingOpRow, PendingOpState},
};

/// Counts of the derived index rows an object holds, one field per derived kind.
///
/// Returned by [`StoreRead::index_row_counts`] for contract verification and
/// diagnostics — the searchable query path is the per-store executor, not this
/// surface. `fts`, `message`, and `event_index` are 0 or 1 (one row per
/// object); the junction counts are unbounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexRowCounts {
    /// Full-text documents (0 or 1).
    pub fts: usize,
    /// Materialized occurrences.
    pub occurrences: usize,
    /// Stored mail rows (0 or 1).
    pub message: usize,
    /// Mail address-junction rows.
    pub addresses: usize,
    /// Membership rows.
    pub memberships: usize,
    /// Event scalar-index rows (0 or 1).
    pub event_index: usize,
    /// Event participant-junction rows.
    pub participants: usize,
}

impl IndexRowCounts {
    /// Returns `true` if the object has no derived rows of any kind.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fts == 0
            && self.occurrences == 0
            && self.message == 0
            && self.addresses == 0
            && self.memberships == 0
            && self.event_index == 0
            && self.participants == 0
    }
}

/// One row of a mail list read: the stored [`MailRow`], the account it belongs to, and the
/// collections it is filed in.
///
/// The account is carried per row because one read spans several of them — an "all inboxes" view
/// is one query, not a loop with a merge in the caller. The mailboxes are carried because a host
/// decides what is in the *viewed* folder, and a message's placement is a separate axis from its
/// identity (JMAP objects hold several memberships; two IMAP copies are distinct objects with one
/// each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailListRow {
    /// The account the message belongs to.
    pub account: AccountId,
    /// The mailboxes/labels the message is filed in.
    pub mailboxes: Vec<MailboxId>,
    /// Every keyword on the message — the system ones the row's `flags` also carries as a
    /// bitfield, plus any user keyword or provider label.
    ///
    /// Carried because this read is the message's **whole mutable state**, not only the part a
    /// list paints: the stored payload deliberately holds none of it, so anything rebuilding a
    /// `Message` from storage gets the complete set here rather than from a second read that
    /// could be forgotten.
    pub keywords: Vec<Keyword>,
    /// The stored row.
    pub mail: MailRow,
}

impl MailListRow {
    /// Projects a normalized message into the list row the store would have written for it.
    ///
    /// A host that watches a sync stream sees whole [`Message`](engine_core::mail::Message)s go by
    /// and has to place them in a list it is already holding. Doing that means projecting them the
    /// way the store does — so it happens here, through the same
    /// [`project_message`](engine_core::search_index::project_message), rather than a second time
    /// in each host with its own idea of what a row's sender or preview is.
    #[must_use]
    pub fn project(account: &AccountId, message: &engine_core::mail::Message) -> Self {
        Self {
            account: account.clone(),
            mailboxes: message.mailboxes.iter().cloned().collect(),
            keywords: message.keywords.iter().cloned().collect(),
            mail: engine_core::search_index::project_message(message).row,
        }
    }
}

/// Where a store's schema stands — what the data is at, what this build expects, and whether
/// opening it moved.
///
/// A support answer, deliberately in the neutral layer rather than in one backend: "which schema
/// is this user's store on, and did this launch upgrade it" is the same question whether the rows
/// live in SQLite, in Postgres, or nowhere at all. A backend with no persistent schema reports
/// `version == expected` and never migrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaStatus {
    /// The schema version the stored data is at now.
    pub version: u32,
    /// The version this build of the engine expects.
    ///
    /// Equal to [`version`](SchemaStatus::version) on a store this build can use. A store left
    /// *ahead* of it — written by a newer build — is refused at open rather than reported here,
    /// because reading it could misinterpret a shape this build does not know.
    pub expected: u32,
    /// The version the store was at when this process opened it, if opening upgraded it.
    ///
    /// `None` when nothing moved: an already-current store, or a freshly created one. A host logs
    /// this once at startup, which is what turns "it broke after the update" into a version pair.
    pub migrated_from: Option<u32>,
}

impl SchemaStatus {
    /// A store that is at the version this build expects and did not migrate on open.
    #[must_use]
    pub fn current(version: u32) -> Self {
        Self {
            version,
            expected: version,
            migrated_from: None,
        }
    }

    /// Whether opening this store upgraded it.
    #[must_use]
    pub fn migrated(&self) -> bool {
        self.migrated_from.is_some()
    }
}

/// Which mail a [`StoreRead::list_mail`] call selects, within the accounts it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailSelector<'a> {
    /// Everything, newest first — the windowed list a host renders.
    Newest,
    /// Every message on any of these threads, regardless of date, so a windowed list can expand a
    /// conversation into its full history. An empty slice selects nothing.
    Threads(&'a [ThreadId]),
    /// The messages named by these provider keys. Keys not found (moved, tombstoned) are simply
    /// absent; an empty slice selects nothing.
    Keys(&'a [ProviderKey]),
    /// The messages filed in one mailbox and dated within `[since, until)`. Undated mail has no
    /// place in a span of time and is not selected; an empty span selects nothing.
    Mailbox {
        /// The mailbox the messages are filed in (among any others).
        mailbox: &'a MailboxId,
        /// The inclusive lower bound.
        since: UtcDateTime,
        /// The exclusive upper bound.
        until: UtcDateTime,
    },
}

/// A minimal lease-free read/inspection surface.
///
/// Enough for the contract suite to verify stored state and for early
/// diagnostics; the structured/full-text query path is a separate sub-step.
#[async_trait]
pub trait StoreRead: Send + Sync {
    /// Where this store's schema stands: the version the data is at, the version this build
    /// expects, and what it was upgraded from if opening moved it.
    ///
    /// Lease-free and cheap — a host calls it at startup to log the store's version, and again
    /// when assembling a diagnostic report.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn schema_status(&self) -> Result<SchemaStatus>;

    /// Every sync scope the store currently knows for `account` (every scope it
    /// has claimed), in ascending [`SyncScope`] order. A per-account search
    /// enumerates these instead of hard-coding which scopes a provider uses, then
    /// routes each by
    /// [`SyncScope::search_domain`](engine_core::sync::SyncScope::search_domain).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>>;

    /// The window a scope's occurrence rows are materialized over, or `None` if nothing has
    /// expanded them yet.
    ///
    /// A lease-free read: the sync and reconcile paths resolve the window they must
    /// re-expand a changed event over *before* claiming the scope
    /// ([`crate::Store::set_expansion_window`]).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn expansion_window(&self, scope: &SyncScope) -> Result<Option<ExpansionWindow>>;

    /// The provider keys of live (non-tombstoned) objects in a scope.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>>;

    /// The stored normalized payload for an object, or `None` if absent or
    /// tombstoned.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>>;

    /// Every live (non-tombstoned) object in a scope as `(provider key, normalized
    /// payload)` pairs, in ascending key order. A batch read for building per-account
    /// views, so a host need not make an N+1 [`object_payload`](StoreRead::object_payload)
    /// call per key.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>>;

    /// The mail rows `select` names across `accounts`, newest first and capped at `limit`
    /// (`usize::MAX` for no cap).
    ///
    /// **This is the read a mailbox list is built from, and no other.** It returns the projected
    /// row — sender, subject, date, flags, preview — so a list costs the rows it shows, not the
    /// mail it is drawn from: a backend answers the first page from an ordered index rather than
    /// by ranking every message in the account and then opening the survivors' payloads. Reading
    /// a body or an attachment list still goes to the normalized object, on demand, for the one
    /// message being opened.
    ///
    /// Several accounts in one call is the point, not a convenience: a unified inbox is a
    /// predicate over one table, so the ordering across accounts is the backend's and not a merge
    /// the caller re-derives. An empty `accounts` selects nothing.
    ///
    /// Whether the account holds a message that is **in the message-id graph but carries no
    /// thread** — the one shape the incremental assignment cannot repair by itself.
    ///
    /// A thread is decided when a message is applied, so this is empty in steady state. It is not
    /// empty right after the migration that introduced the graph: that step backfills the graph
    /// rows from the stored payloads but assigns no thread ids, so any message the *old*
    /// whole-account pass had not yet grouped stays ungrouped — and a later arrival cannot adopt
    /// it, because the component lookup reaches a stored message only through the thread id its
    /// row already carries. Its replies would open conversations of their own, quietly, forever.
    ///
    /// Deliberately a question about the damage rather than a flag saying repair is due: a flag
    /// has to be set by whoever knew, and cleared by whoever fixed it, and is wrong if either
    /// forgets. This is true exactly when there is something to fix, so it also catches a rebuild
    /// that failed halfway and a store damaged some way nobody predicted. `engine-sync` asks it
    /// once per mail sync and repairs when the answer is yes — no host is asked to remember
    /// anything.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn has_ungrouped_graphed_mail(&self, account: &AccountId) -> Result<bool>;

    /// Rows sort newest first with undated messages last — they enter a window only if dated ones
    /// leave room — and the order is **total**: ties break on the row's own identity, so two
    /// reads of an unchanged store return the same sequence and a host reconciling by row id sees
    /// no movement.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn list_mail(
        &self,
        accounts: &[AccountId],
        select: MailSelector<'_>,
        limit: usize,
    ) -> Result<Vec<MailListRow>>;

    /// The date of the oldest message `account` holds in `mailbox`: how far back this store
    /// reaches for that mailbox, whatever window synced it. `None` when the mailbox holds no
    /// dated mail.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn oldest_in_mailbox(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> Result<Option<UtcDateTime>>;

    /// The materialized occurrences in a scope that overlap `window`, ascending by
    /// `(start, end, event)`.
    ///
    /// This is the range read a calendar grid pages over: recurrence lives in the
    /// occurrence rows, not the master event (`store-and-sync.md`), so a host that
    /// reads the events alone sees a weekly meeting once, at the series start. The
    /// window is half-open at **both** ends ([`Horizon::overlaps`]), so an event
    /// ending exactly when a week opens belongs to the previous page only, and a
    /// multi-day event that merely *covers* the window is still returned — it has to
    /// render on every day it spans.
    ///
    /// Order is **specified**, unlike a mail list read's tie-breaking,
    /// because a host lays these rows out geometrically: two hosts given the same window
    /// must place an overlapping event in the same column, and an unstable order would
    /// silently make them disagree. Empty for a non-calendar scope (only events expand)
    /// and for a scope the store has never seen. Occurrences are cleared when their
    /// event is tombstoned, so the rows are exactly the live events' — no liveness
    /// join is needed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn scope_occurrences(
        &self,
        scope: &SyncScope,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>>;

    /// The current lifecycle state of a pending op, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>>;

    /// Every outbox row for `account` that has not settled: what is queued, what is in
    /// flight, what is waiting out a retry, and what is awaiting confirmation.
    ///
    /// This is what makes the outbox a queue rather than a set of ids a caller had to
    /// remember. [`pending_op_state`](Self::pending_op_state) answers about one op the
    /// caller already holds the id of, which a host restarting does not; without an
    /// enumeration it cannot show a user what is still unsent, and cannot offer to
    /// withdraw it.
    ///
    /// Settled rows are excluded: they are kept forever as the idempotency record
    /// (`store-and-sync.md`), so returning them would grow without bound and say nothing
    /// about what is outstanding. Ordered by id, which is enqueue order.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn list_pending_ops(&self, account: AccountId) -> Result<Vec<PendingOpRow>>;

    /// The counts of derived index rows currently stored for an object, across
    /// every derived kind. Zero for an absent or fully-tombstoned object.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts>;
}
