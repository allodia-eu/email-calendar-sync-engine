# Store and Sync Concurrency Contract

This document fixes the concurrency, transaction, and lease semantics of the
`Store` trait and the sync/outbox workers that drive it. `north-star.md` states
the Store Contract guarantees at a high level; this document is the
authoritative source for the trait signature and its concurrency model. Read it
before working in `engine-store`, `store-sqlite`, or `engine-sync`.

## Scope

Covered here: what a sync scope is, how writers are serialized, what commits
atomically, and how the outbox claims and resolves work.

Out of scope (owned elsewhere): object identity and membership
(`modeling.md`), provider cursor formats and capability detection
(`providers.md`), and the search/index data model (`north-star.md` Search
Contract). This document only constrains *when and under what lock* those land.

## Principles

- **At most one effective writer per scope, and per in-flight op, at a time.**
  Enforced by store-issued fencing tokens checked inside the write
  transaction — never by trusting a worker to behave.
- **Every durable state transition is lease-gated and atomic.** Provider data,
  derived search/occurrence rows, the next cursor, reconciliations, and
  tombstones for one scope commit together or not at all.
- **The store is mechanical.** It performs no normalization, text extraction, or
  recurrence expansion. All such work is done by pure `engine-core` /
  `engine-recurrence` functions *before* the store call; the store writes the
  result. (Occurrence expansion is `engine_recurrence::expand`; text/structured
  projection is `engine_core::search_index`.)
- `engine-core` stays I/O-free and async-free. Async and I/O live only in store
  implementations and provider crates.

## SyncScope

A scope is the unit of sync state, leasing, and serialization. Its granularity
is dictated by the protocol, and the three protocols disagree — so `SyncScope`
is an enum, not a single id:

- **JMAP:** state is **per account, per data type** (`Email/changes`,
  `Mailbox/changes`, `CalendarEvent/changes`, … each carry their own state
  string). There is no per-mailbox email state. Scope = `(account, JmapType)`.
- **IMAP:** state is **per mailbox** (`UIDVALIDITY`, `UIDNEXT`, and
  `HIGHESTMODSEQ` under CONDSTORE). Scope = `(account, MailboxKey)`.
- **CalDAV/CardDAV:** state is **per collection** (RFC 6578 sync-token, or
  CTag + per-resource ETags). Scope = `(account, CollectionKey)`.
- **SMTP** is not a sync scope. It is an outbox transport only; the outbox is
  leased per account (see below).

Consequences that the orchestrator must not paper over:

- **Lease cardinality differs by provider.** A JMAP account syncs under a few
  coarse leases (one per type); an IMAP account under many fine leases (one per
  mailbox). Do not assume a fixed fan-out per account.
- **Referential apply order.** Container scopes (mailboxes, calendars, address
  books) are applied before the member scopes that reference them (emails,
  events, contacts). Membership rows resolve against already-applied containers,
  and snapshot tombstoning of a container set precedes member tombstoning.
- The cursor inside `SyncState` is opaque and provider-specific. The engine
  never parses it; it only round-trips it through the store.

## Leases and fencing tokens

There is one serialization mechanism, not two: **a store-issued lease carrying a
monotonic fencing token; a write is admitted iff its token is still current for
the scope.** The fencing token *is* the compare-and-swap key — leasing and CAS
are one mechanism here, not alternatives.

- `claim_sync_scope` atomically acquires the lease and returns the current
  `SyncState`, so the planner sees a consistent `(lease, state)` pair with no
  load-then-claim race. `load_sync_state` is a lease-free read for diagnostics
  and UI only; never plan a write from it.
- Each claim bumps the scope's stored fencing generation. An older lease is now
  stale. `apply_sync_update` and `apply_maintenance` re-check the token **inside
  the transaction** and fail with `StaleLease` if it is not current.
- Leases have a TTL (host-tunable via the injected clock). This matters most on
  mobile: an app suspended mid-sync sails past its TTL, another worker re-claims
  and bumps the generation, and when the suspended worker resumes its apply is
  rejected as stale instead of corrupting state. Workers handle `StaleLease` by
  re-claiming and recomputing — never by retrying the stale write.
- `release_sync_scope` frees a scope before its TTL so a finished worker does not
  block the next sync for the full lease window.

## The atomic apply

`apply_sync_update` commits exactly one transaction for one scope, gated by the
lease token. The transaction contains, all-or-nothing:

1. Normalized provider objects and their preserved raw payloads.
2. Membership rows.
3. Derived FTS rows (from extracted text) and structured-filter rows — the scalar
   index rows and the address/participant/membership junctions that back the
   non-text filters.
4. Derived `event_occurrence` rows within the current horizon.
5. The next `SyncState` (cursor).
6. Pending-op reconciliations.
7. Tombstones for snapshot reconciliation.

Items 3–4 are **precomputed by pure engine code before the call** and carried in
the batch; the store does not compute them. This keeps the transaction short
(no expansion under lock) and the store logic-free. The batch is one struct so
the atomic set is self-documenting:

```rust
pub struct ApplyBatch<'a, T> {                  // T is the scope's StorableObject
    pub update: &'a SyncUpdate<T>,              // provider-normalized objects, raw, membership
    pub derived: &'a DerivedWrite,              // FTS + structured-filter + occurrence rows, pure engine fns
    pub reconcile: &'a [PendingReconciliation],
    pub next_state: Option<&'a SyncState>,      // Some => advance cursor; None => leave it (streaming page)
}
```

- **Cursor disposition.** `next_state` is `Some(state)` to advance the scope
  cursor on commit (the normal case; `ApplyBatch::new`), or `None` to apply the
  objects/derived rows but **leave the cursor unchanged** (`ApplyBatch::with_cursor`).
  `None` is for **incremental/streaming** applies: a paged fetch commits each page
  additively (objects become visible immediately) without yet marking the scope
  synced, then one final apply carries the real `Some(cursor)`. Crucially, a
  *snapshot* pass must not tombstone against one page's partial id set, so the
  orchestrator applies intermediate snapshot pages as **additive deltas** (upsert,
  no removals) while accumulating `present` across pages, and only the final page
  applies the real `Snapshot` with the complete `present` set — so it tombstones
  exactly the genuinely-absent rows, never an earlier page's. A crash mid-stream
  therefore leaves the prior cursor intact, so the next sync re-runs the pass from
  scratch idempotently rather than skipping the un-applied pages. This orchestration
  lives in `engine-sync::sync_mail_streamed` (which also reports per-page
  `SyncProgress` to a `ProgressSink`); the contract suite's
  `streaming_page_keeps_cursor` locks the store primitive for every backend.

- **Delta vs snapshot.** `SyncUpdate` is either a delta or a bounded/full
  snapshot. A snapshot carries the complete current provider-id set for its
  scope; the store tombstones local rows in that scope absent from the set.
  `cannotCalculateChanges` (JMAP) and a UIDVALIDITY reset (IMAP) produce
  snapshots, not deltas.
- **Reconciliation is re-validated in the transaction.** Matching an incoming
  object to an outstanding send (by generated `Message-ID`) is planned off the
  transaction by reading pending ops, so there is a TOCTOU window. Inside the
  apply transaction the store re-checks that each `PendingReconciliation`
  references an op still in its expected pre-resolution state. On mismatch it
  **skips** that reconciliation and stores the incoming object normally;
  duplicate suppression then falls back to presentation-layer dedup
  (consistent with "UI/search dedup is presentation policy, not storage
  identity").
- **Idempotent replay.** Re-applying the same batch after a crash is a no-op:
  object writes are upserts keyed by provider key, the cursor advance is
  conditional on the prior state, and a resurrected stale-token worker is
  rejected before it can write.

## Derived-data maintenance (writes not driven by sync)

Some FTS/occurrence writes do not come from a sync cycle and must obey the same
discipline:

- The rolling occurrence **horizon advances** over time.
- **Timezone data changes** invalidate already-materialized occurrences.
- A **Tier-3 body fetched on demand** must be indexed so it becomes searchable.

These go through `apply_maintenance`, which writes only derived rows under the
**same scope lease** as sync — so maintenance and sync of one scope cannot race.
A cross-cutting trigger (a tzdata bump) fans out by acquiring each affected
scope's lease in turn.

For the occurrence triggers, the per-scope step is: re-run `engine_recurrence::expand`
for the scope's events over the (advanced) horizon with the current tzdata, then
commit a maintenance batch through `apply_maintenance`. Because `DerivedWrite::removed`
clears **every** derived kind for a key — not just occurrences — the batch re-derives
each event in full: `removed: [event keys]` plus a fresh projection
(`push_event`/`push_mail`) **and** the fresh occurrences, so a horizon advance does
not strip an event's FTS/structured rows. `removed`-before-upserts makes the replace
atomic, and unchanged occurrence instants stay byte-stable. `engine-cli`'s
`reexpand_calendar` is the worked example. The cross-scope fan-out driven from sync
state — plus a `tzdata-version` index to find *only* stale scopes, and an
occurrence-only clear so a pure horizon advance need not re-project unchanged text —
is the sync orchestrator's job, a later step.

On-demand fetched bodies **are** indexed (resolving the "does opening old mail
make it searchable?" question: yes). Search coverage metadata must therefore
reflect that local coverage can grow over time; it is not a static property of
the corpus.

## The outbox

Pending ops are durable before any side effect and are claimed with the same
fencing discipline as scopes.

- **Enqueue is idempotent.** Every `PendingOp` carries a client
  `idempotency_key`. Re-enqueuing the same key (e.g. after a crash between the
  side effect's commit and the caller learning its id) returns the existing
  `PendingOpId` instead of creating a duplicate.
- **Claim returns only runnable ops.** `claim_pending_ops` excludes any op whose
  `depends_on` are not all in a terminal-success state, and any op whose
  `resource_key` collides with an already-leased op. This both honors offline
  `create → edit` dependency chains (the edit waits until the create's provider
  id is known) and serializes writes to the same provider resource.
- **Resolution is fenced.** Each claimed op is leased individually with its own
  fencing token (`OpLease`). `mark_pending_op` takes the `OpLease`, not a bare
  id, and the store rejects a stale token. The outbox path is fenced exactly
  like the sync path: a sync-only fence would let a suspended-then-resumed mobile
  worker clobber an op that was already re-claimed.
- The outbox lease is **account-scoped**, independent of sync scopes.

## Revised trait

```rust
#[async_trait]
pub trait Store: Send + Sync {
    // Read-only inspection. Never plan a write from this.
    async fn load_sync_state(
        &self,
        account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>>;

    // Sync writer path. The lease pins (account, scope) + fencing token, so the
    // apply call carries no loose account/scope args to disagree with it.
    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim>; // { lease, state: Option<SyncState> }

    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: StorableObject + Serialize + Send + Sync;

    async fn apply_maintenance(
        &self,
        lease: &SyncLease,
        derived: &DerivedWrite,
    ) -> Result<()>;

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()>;

    // Outbox.
    async fn enqueue_pending_op(
        &self,
        account: AccountId,
        op: PendingOp,
    ) -> Result<PendingOpId>; // idempotent by (account, op key); PendingOp carries no account
    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>>; // runnable ops only

    async fn mark_pending_op(
        &self,
        lease: &OpLease,
        outcome: PendingOutcome,
    ) -> Result<()>;
}
```

The provider-neutral sync data shapes (`SyncScope`, `SyncState`, `SyncUpdate`,
`PendingOp`, `PendingOutcome`) live in `engine-core`; the lease, batch, and
fencing vocabulary lives in `engine-store`, beside the trait that issues it. The
trait is **encryption-agnostic** — at-rest encryption is a `store-sqlite`
construction detail (plain SQLite over OS file encryption by default, SQLCipher
opt-in), so the same contract holds either way. A small `StoreRead` companion
(lease-free object/key inspection) backs the contract suite and early reads.

Supporting types (abbreviated):

- `SyncScope` — enum over `JmapType { account, ty }`, `ImapMailbox { account, mailbox }`, `DavCollection { account, collection }`.
- `SyncLease` / `OpLease` — opaque, store-issued; expose fencing token, bound identity, and expiry.
- `StorableObject` — the trait domain objects implement so the store keys and persists them mechanically; `ApplyBatch<'a, T>` and `apply_sync_update` are generic over it.
- `DerivedWrite` — precomputed FTS rows, structured-filter rows (scalar index rows
  plus the address/participant/membership junctions), and bounded
  `event_occurrence` rows, plus their tombstones; the store writes them, never
  computes them. The full-text and structured rows are projected by pure
  `engine-core` functions (`engine_core::search_index::{project_message, project_event}`,
  carried in via `DerivedWrite::push_mail`/`push_event`); occurrence rows come from
  `engine_recurrence::expand`, and each carries the `tzdata_version` it was expanded
  under (so a tzdata bump can find and re-expand exactly the affected rows). Junction
  and scalar rows **replace** per object on replay (idempotent), and
  `DerivedWrite::removed` is applied **before** the upserts, so a re-expansion batch
  (`{ removed: [event], occurrences: [fresh] }`) clears an event's stale occurrences
  and writes the fresh ones in one transaction. A small
  `StoreRead::index_row_counts` inspection backs the shared contract's structured-row
  parity case.
- `LeaseRequest { owner: WorkerId, ttl: Duration }`.
- `PendingOp { idempotency_key, depends_on: Vec<PendingOpId>, resource_key: ResourceKey, payload }`.
- `PendingOutcome` — `Succeeded { provider_key }` | `Failed { class, retry_after }` | `NeedsConfirmation { .. }`.

## Error classification

Store errors map onto the provider taxonomy in `providers.md`:

- `StaleLease` — token superseded; **not** retryable as-is. Re-claim, recompute,
  reapply.
- `ScopeHeld` — a live lease exists; retryable after backoff.
- `Conflict` — optimistic write conflict surfaced from the store (e.g. snapshot
  vs concurrent delta); recompute.
- `NotRunnable` — an op was asked to resolve but its dependencies regressed.

## Required tests

Lock these as failing tests before implementing the store:

- A write under a superseded lease is rejected with `StaleLease`; the winning
  writer's data is intact.
- `mark_pending_op` under an expired `OpLease` is rejected after the op was
  re-claimed.
- `mark_pending_op` records `Failed` and `NeedsConfirmation` outcomes distinctly,
  and a lease naming an op with no row is rejected as `StaleLease` (not silently
  applied); an unknown op id reads back no state.
- `claim_pending_ops` never returns an op with unsatisfied `depends_on`, nor two
  ops sharing a `resource_key`, and returns at most `limit` runnable ops.
- Re-enqueue with a duplicate `idempotency_key` returns the original id and
  creates no second op.
- Replaying an identical `ApplyBatch` after simulated crash leaves identical
  state (idempotent).
- A snapshot `SyncUpdate` tombstones exactly the local rows absent from its id
  set, and nothing else.
- A `PendingReconciliation` whose op changed state between planning and apply is
  skipped, and the incoming object is stored without loss; one whose op is still
  in its expected state resolves the op to `Succeeded` in the apply transaction.
- A `release_sync_scope` under a superseded lease is a no-op and does not free a
  scope a newer lease holds.
- Container-before-member apply ordering holds, including under snapshot
  tombstoning. (The store enforces per-scope snapshot tombstoning and keeps
  scopes independent; the cross-scope *apply order* itself is an orchestrator
  invariant, locked in `engine-sync` rather than in the store.)
