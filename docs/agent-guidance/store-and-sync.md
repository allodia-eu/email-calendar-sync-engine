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

What it does **not** constrain is what any of it costs. The suite's mailboxes hold
single digits of messages, where a full scan and an indexed seek measure the same.
Before changing a read path, an apply, or the derivation pass, take a baseline against a
mailbox that can tell them apart: `cargo bench -p mailbox-fixture` (`rust.md` →
"Measuring at scale").

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

## One writer, a pool of readers

WAL admits one writer and many readers at once, so a store that funnels both
through a single connection makes a committing sync block the list read a user is
waiting on. `store-sqlite` therefore opens a writer plus a small pool of readers
for a file database (`pool.rs`), and each call site picks: transactions and
pragmas take `call`, everything else takes `read`.

Three things hold that split honest, and each is load-bearing:

- **Readers are `query_only`.** A write handed to `read` fails outright rather
  than quietly taking a reader's lock and serializing again. Routing is a
  judgement per call site; a judgement nothing checks is one that drifts.
- **The contract suite runs on disk as well as in memory.** An in-memory database
  is a single connection — one connection *is* the database, so a second would be
  a different, empty one — which means `:memory:` alone exercises no routing at
  all.
- **Every statement goes through `sql::execute` / `query_opt` / `query_all`**, so
  it is compiled once per connection instead of on every call. The point queries
  behind a windowed read and the upserts behind a sync page are the same dozen
  statements run thousands of times, and re-parsing them was most of what they
  cost.

An index is part of the query that uses it. Adding one without the read that plans
through it is write cost for no read benefit, and no test can tell the difference —
a list read returns the same rows scanned or seeked. Assert the plan
(`EXPLAIN QUERY PLAN`) when you add an index, and add it in the same change as its
reader. Where the planner's choice decides whether a read costs the page or the
mailbox, **name the index** (`INDEXED BY`) rather than hoping: SQLite plans this schema
unanalysed, and the failure mode is a correct answer arrived at by sorting everything.

## The mail list is a table, not a scan

`message` holds one row per stored mail object carrying exactly what a list row, a sort,
a conversation group and a date filter need — `(account, scope_key, provider_key)`,
`thread_id`, `message_id`, `date_utc`, the `flags` bitfield, `has_attachment`, and the
row's visible text (`from_name`, `from_addr`, `subject`, `preview`). `object.payload`
stays the canonical normalized record and is off the list path entirely: opening a
message reads it, showing a message in a list does not.

`StoreRead::list_mail(accounts, selector, limit)` is that read, and the only one a
mailbox list is built from. Three selectors, one projection:

- `Newest` — one ordered statement over every account named. **A unified inbox is a
  predicate, not a loop with a merge above it**: the ordering across accounts is the
  backend's, so two accounts cannot be interleaved differently by two callers.
- `Threads(&[ThreadId])` and `Keys(&[ProviderKey])` — targeted seeks, one cached
  statement per value, against `message_account_thread` / `message_account_key`.

Order is `date_utc DESC` with undated mail last, ties broken on the row's own identity
so the window a `LIMIT` cuts is the same on every read of an unchanged store — a host
reconciling by row id must not see movement a data change did not cause.

`flags` carries the four RFC 8621 system keywords a row's appearance depends on
(`$seen`, `$flagged`, `$draft`, `$answered`), because a sort or a filter must not pay a
join for them. Every keyword, system and user alike, still lands in `membership`, which
is what `keyword:` searches and where a set of arbitrary cardinality belongs.

## SyncScope

A scope is the unit of sync state, leasing, and serialization. Its granularity
is dictated by the protocol, and the three protocols disagree — so `SyncScope`
is an enum, not a single id:

- **JMAP:** state is **per account, per data type** (`Email/changes`,
  `Mailbox/changes`, `CalendarEvent/changes`, … each carry their own state
  string). There is no per-mailbox email state. Scope = `(account, JmapType)`.
- **IMAP:** email state is **per mailbox** (`UIDVALIDITY`, `UIDNEXT`, and
  `HIGHESTMODSEQ` under CONDSTORE). Scope = `(account, MailboxKey)`
  (`ImapMailbox`). The account's **folder list** is a separate per-account
  container scope, `ImapMailboxList{account}` — a `LIST` re-snapshots it each pass
  (no folder-list cursor), applied before the per-mailbox email it parents
  (`imap-smtp.md`).
- **CalDAV/CardDAV:** state is **per collection** (RFC 6578 sync-token, or
  CTag + per-resource ETags). Scope = `(account, CollectionKey)`
  (`DavCollection`). The account's **collection list** (calendar/address-book
  discovery) is a separate per-account container scope, `DavCollectionList{account}`
  — a `PROPFIND` of the home re-snapshots it each pass (no list cursor), applied
  before the per-collection members it parents (`caldav.md`), exactly as
  `ImapMailboxList` parents `ImapMailbox`.
- **SMTP** is not a sync scope. It is an outbox transport only; the outbox is
  leased per account (see below).
- **Push (IMAP `IDLE`) is not a sync scope either.** A `Watch` session
  (`providers.md`, `imap-smtp.md`) only signals that a scope *may* have changed; the
  host responds by running that scope's normal sync, under the same lease and atomic
  apply as a poll. Push adds no lease semantics and writes nothing itself — it is a
  latency optimization over polling, and the scope sync stays the authoritative,
  idempotent reconciliation.

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
- **The claim → fetch → apply → release cycle lives in exactly one place**:
  `engine_sync::run_scope`, parameterized by `ScopeSyncer`. Every scope — mailboxes,
  email, calendars, events, address books, contact cards — goes through it. This is
  the part that is easy to get subtly wrong (release on fetch failure so a leaked
  lease does not become a spurious `ScopeHeld`; re-claim rather than retry on
  `StaleLease`), so a scope with an extra requirement **extends the trait, it does not
  copy the loop**. The seams that exist for that: `ScopeSyncer::observations` (extra
  rows to write in the *same* transaction — email's recipient observations use it),
  `ScopeFetch::Halt`/`ScopeRun::Halted` (a fetch that declines to produce a batch —
  an unavailable contact source), and the `Meta` associated type (per-fetch
  information carried back to the caller, e.g. "the cursor was rebuilt"). Bookkeeping
  a specific scope needs *around* the loop (contact-source availability) belongs in a
  thin wrapper, not inside the driver.
- `abandon_sync_leases` is the explicit **process-startup recovery** primitive for
  a host that knows prior workers for the store are gone after abrupt termination.
  It clears held sync leases, preserves cursors and objects, and bumps each
  affected fencing token so an abandoned worker cannot later commit under its old
  lease. It is not an in-process contention workaround: a live `ScopeHeld` still
  means "retry after the current worker finishes."

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
8. Recipient observations derived from changed messages in a resolved Sent
   collection. Their `(account, source message, canonical email)` identity is
   committed with the message/cursor so replay cannot inflate counts.

Contact-card applies also advance a contact-source generation. Unified people
are rebuilt from a consistent generation and atomically replaced only when that
generation is still current; a raced rebuild retries. Provider source rows are
never coalesced. Stable `PersonId` assignment, merge aliases, split retention,
history suppression, and the one-time existing-message interaction backfill are
owned by `ContactStore`; `contacts.md` is authoritative.

Contact-photo metadata is stored by `(account, contact)` with its provider
fingerprint and content hash. Bytes use the content-addressed blob area;
fingerprint mismatch or a missing/corrupt blob is a cache miss.

Items 3–4 are **precomputed by pure engine code before the call** and carried in
the batch; the store does not compute them. This keeps the transaction short
(no expansion under lock) and the store logic-free. The batch is one struct so
the atomic set is self-documenting:

```rust
pub struct ApplyBatch<'a, T> {                  // T is the scope's SyncObject
    pub update: &'a SyncUpdate<T>,              // provider-normalized objects, raw, membership
    pub derived: &'a DerivedWrite,              // FTS + structured-filter + occurrence rows, pure engine fns
    pub reconcile: &'a [PendingReconciliation],
    pub next_state: Option<&'a SyncState>,      // Some => advance cursor; None => leave it (streaming page)
}
```

- **Cursor disposition.** `next_state` is `Some(state)` to advance the scope
  cursor on commit (the normal case; `ApplyBatch::new`), or `None` to apply the
  objects/derived rows but **leave the cursor unchanged** (`ApplyBatch::with_cursor`).
  The non-streaming whole-scope apply always advances. A **streaming** apply picks
  its disposition per chunk from the chunk's `PassMode` (see **Streaming sync**
  below): an *additive* chunk passes `Some(checkpoint)` so the cursor advances with
  every commit (resumable), while a *reconcile* chunk passes `None` until its final
  chunk, which carries the real `Some(cursor)` and tombstones. So the store
  primitive is unchanged; only which disposition the orchestrator picks per chunk is
  new. The contract suite's `streaming_page_keeps_cursor` locks the `with_cursor`
  primitive for every backend.

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
- **A change is whole, partial, or a removal.** `SyncUpdate::Delta` carries all three:
  `changed` (whole objects), `patched` (partials), `removed`. A partial names the fields that
  moved and nothing else, so the store writes those columns and leaves the rest — including the
  normalized payload — alone. The partial form is per object type, `SyncObject::Patch`; it is
  `MailStateChange` for a message and the uninhabited `NoPatch` for everything else, so
  `Vec<NoPatch>` makes "a calendar pass carries no partials" a fact the compiler checks.

  A key present in both `changed` and `patched` is resolved **in favour of `changed`**, inside
  `with_patched`: a whole object is strictly more information, and an adapter fetches it after
  learning of the change, so it is the later word. Gmail's history API produces both for one id
  in a single page, so this is a real case. Resolving it centrally means no adapter has to
  remember to and no store has to guess.

  A snapshot has no partials by construction — it is the scope's whole current state.

- **A message is an immutable half and a mutable half, and they are stored apart.** The content
  never changes once the server holds it — editing a draft mints a *new* provider object on every
  protocol we speak (a JMAP `Email` is immutable; IMAP does APPEND + EXPUNGE and the UID changes),
  so that half can be written once and never reconciled. It is `MailContent`, and it is the
  normalized payload.

  Everything that moves *without* those bytes moving is `MailState`: keywords, a derived thread,
  and the revision tokens plus `last_modified` that bump when any of it changes. Its home is the
  `message` row and the `membership` junction. A payload carrying a copy could only ever be a copy
  that disagrees.

  **Adding a state axis is adding a field to `MailState`**, not a new mechanism — Graph's
  `categories` is the next one, and it needs that field plus a `membership` kind. A thread is
  present in the payload only when the **provider** assigned it, because then it is the provider's
  word; a derived one is the engine's and lives in the row alone.

  This is why a mark-read can no longer destroy anything: there is no second copy to overwrite.
  It replaces the old carry-forward, which refilled `thread` and `preview` from the stored copy
  before every apply because the apply was about to replace the whole payload.

  `MailContent` is built by **destructuring** `Message`, so a new field on `Message` stops the
  conversion compiling until someone decides whether it belongs in the payload or in the row. A
  silent default is exactly the failure the type exists to prevent.

  Reading is the mirror: a `Message` decoded from a payload alone is incomplete by construction,
  and `Engine::compose` rebuilds its state from the row and the junction. Every
  path from storage back to a `Message` goes through it. A provider-assigned thread that came
  back with the payload is left as it is — relabelling it derived would tell the derivation pass
  it may re-thread mail the provider threaded.
- **Idempotent replay.** Re-applying the same batch after a crash is a no-op:
  object writes are upserts keyed by provider key, the cursor advance is
  conditional on the prior state, and a resurrected stale-token worker is
  rejected before it can write.

## Streaming sync: additive checkpointing vs reconcile

The responsive mail path (`engine-sync::sync_mail_streamed` / `sync_email_streamed`,
driven from `Engine::sync_mail_streamed` / `sync_folder_email_streamed`) commits mail
**chunk by chunk** under one lease, so a host renders recent mail and live progress
before a pass finishes. The provider primitive is `Provider::stream_email`
(`providers.md`): a pull `EmailStream` of `EmailChunk`s. Each chunk carries a
`PassMode` (constant across the pass), its `changed` upserts, explicit `removed` keys,
a `present` id set (reconcile only), an optional `total`, and an `advance_to` cursor
disposition. The two modes differ in **resumability**:

- **Additive** — a first cold backfill, and every steady-state delta. Nothing local
  needs tombstoning (a first sync stored nothing; a delta reports its own removals),
  so each chunk applies as a `Delta { changed, removed }` and **advances the cursor to
  its own `advance_to` checkpoint** (`Some(cursor)` on every chunk). A crash therefore
  resumes from the last committed checkpoint instead of re-downloading from the start:
  the IMAP cold backfill checkpoints its lowest-committed UID per fetch group
  (`imap-smtp.md`), so a killed backfill of a large mailbox continues below the
  watermark. (A JMAP/Graph backfill is fast HTTP paging that is not cheaply resumable
  mid-pass, so its intermediate chunks are *held* — visible but no checkpoint — and a
  final marker chunk carries the cursor; a crash there re-runs the pass.)
- **Reconcile** — a re-snapshot: an IMAP `UIDVALIDITY` reset, a JMAP
  `cannotCalculateChanges`. Local rows exist and must be reconciled against the
  server's current set, so intermediate chunks apply **additively** (upsert, no
  removals) while the orchestrator accumulates `present` across chunks and **holds the
  cursor** (`None`); only the final chunk applies the real `Snapshot` with the complete
  accumulated `present` set — tombstoning exactly the genuinely-absent rows, never an
  earlier chunk's — and advances the cursor in one commit. It is **not**
  checkpoint-resumable (a crash re-runs the pass), acceptable because a re-snapshot is
  the rare path. A crash mid-reconcile leaves the prior cursor intact, so the next sync
  re-runs it idempotently.

**Two decoupled knobs (`StreamTuning { window, fetch_batch, chunk_size }`).** A single
page size used to conflate network batching with commit granularity. `fetch_batch`
bounds each provider round trip (an IMAP `UID FETCH` window, a JMAP `Email/get` page, a
Graph `$top`); `chunk_size` bounds how many messages accumulate before a chunk is
committed and reported. A large `fetch_batch` with a small `chunk_size` gives *both*
few round trips *and* row-as-it-arrives commits (`StreamTuning::responsive` is the
interactive default, `bulk` the throughput one). `window` is the per-sync **depth**
(`SyncWindow { since }`, `engine-core`) — a provider-neutral date floor bounding a
snapshot/backfill (a delta is new-arrivals-only and never narrowed), passed per sync so
a host changes depth without reconnecting providers.

**Change events (`SyncObserver` / `SyncCommit`).** After every committed chunk the
orchestrator calls the caller's `SyncObserver::committed` with a `SyncCommit { scope,
fetched, total, upserted, removed }` — the running progress *and* the exact rows that
just landed (borrowed, zero-copy on the engine side). A host splices its view from
`upserted`/`removed` **without re-querying** the mailbox. (Reconcile tombstones are
computed by present-set diff inside the store, so they are not enumerated in `removed`;
the pass's `SyncApplied::tombstoned` count signals a host may re-read to reconcile.)
`IgnoreCommits` is the no-op sink, and a closure is a `SyncObserver` via the blanket
impl. `AccountProgress` is a ready-made observer that folds per-scope commits into one
account-level "downloaded Y of X" over a concurrent per-folder fan-out — keeping the
total indeterminate until every expected scope has reported one (so the bar never
rebases upward as later folders start) and tracking in-flight passes via `begin`/`finish`.

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
atomic, and unchanged occurrence instants stay byte-stable.

`engine_sync::expand_calendar_horizon` implements this, fanning out across the
account's event scopes (enumerated by `SyncScope::object_kind`, so a CalDAV account's
one-scope-per-collection and a JMAP account's single event type are both handled), and
is exposed on the facade as `Engine::expand_horizon`.

## The expansion window is the store's, not the caller's

`event_occurrence` rows are keyed by `(scope, event, start, recurrence-id)` and **upserted**,
so a changed event whose key set moved — a start that shifted, an `RRULE` that shrank — would
*add* rows beside its stale ones rather than replace them, and render at both instants on a
grid forever. So `EventScope::derive` clears each changed event's occurrence rows before
re-deriving them (`DerivedWrite::reset_occurrences` — the narrow counterpart of `removed`:
every other derived kind is a single upserted row or a per-object junction replace, so
clearing those too would be churn). This is not a calendar-write concern; a *remote* move
mis-synced the same way.

But that clear is **unwindowed**, and that is what forces the rest of this design. The rows
were always *relative to* a horizon, and the horizon was implicit — so re-expanding a cleared
event over whatever horizon the **caller** happened to hold silently destroyed everything
outside it: a routine one-month delta that touched one event deleted that event's whole
already-expanded year, while every *unchanged* event kept theirs. A weekly meeting vanished
from next month's grid because somebody renamed it, and no host signal said to re-expand.

So the store owns the window. `sync_scope` records the `ExpansionWindow` — the horizon its
occurrence rows span and the zone they were resolved through (schema v6) —
`Store::set_expansion_window` writes it under the scope lease, and
`StoreRead::expansion_window` reads it back:

- **`expand_calendar_horizon` is the only call that moves it**, and it has just re-expanded
  *every* event in the scope to match, so recording it there is honest by construction.
- **A sync re-expands a changed event over the stored window**, never over its own `horizon`
  argument — which now only *seeds* the window on the very first sync of a scope, so a host
  that has not called `expand_horizon` yet still materializes something.
- **A post-write reconcile takes the same window**, so `Engine`'s calendar writes need no
  `horizon`/`host_zone` at all: a write is not the place to state what the UI is showing.
  Reconciling a scope that has never been expanded is `SyncError::NoExpansionWindow` rather
  than a silent no-op — expanding nothing would store the events with zero occurrences *and*
  advance the cursor, leaving the grid confidently empty forever.

**A sync will not do this, and that is the trap.** `ScopeSyncer::derive` expands only
the objects the delta *changed*, so once an account is synced, a provider reporting "no
changes" (the steady state) derives no occurrences at all. A host that widens its
horizon and re-syncs to fill the new range gets **nothing, permanently** — the range
read over it returns empty, and syncing again never fixes it. Only a maintenance
re-expansion (or a full reset) materializes it. The same applies to a **host-zone
change**: a floating event's stored `start_utc` is only correct for the zone it was
expanded under.

Still deferred: a `tzdata-version` index to find *only* stale scopes, and an
occurrence-only clear so a pure horizon advance need not re-project unchanged text.
Today `expand_calendar_horizon` re-expands every event in the scope on every call, so a
host widens in coarse chunks against its own watermark rather than calling it per page.

On-demand fetched bodies **are** searchable (resolving the "does opening old mail
make it searchable?" question: yes), via a separate lease-free body index — **not**
the scope-derived FTS — so opening a message indexes its body immediately and a
later re-snapshot cannot wipe it (below). Search coverage metadata must therefore
reflect that local coverage can grow over time; it is not a static property of the
corpus.

## On-demand message content: text vs bytes (Tier-3 bodies)

A message's on-demand content splits by **text vs bytes** (`north-star.md`):
searchable text in SQLite, the heavy byte payload on the filesystem. Both are cached
through **separate, lease-free** traits in `engine-store` (beside `Store`), keyed by
`(account, ProviderKey)`:

- They sit **outside** the scope-fencing/lease contract on purpose. The raw bytes for
  a `(UIDVALIDITY, UID)` (or JMAP blob) are immutable and the extracted text is a pure
  function of them, so the caches are idempotent and need no lease — a host opens and
  searches a message *while a sync of its scope is in flight*; taking the scope lease
  would needlessly serialize reads behind sync.
- **Bytes — `MessageSourceCache`** (`put_message_source`/`get_message_source`). The
  raw RFC 5322 source (the whole `BODY.PEEK[]`, which carries the attachments) can be
  1–15 MB, so it does **not** live in SQLite. `store-sqlite` writes it to a
  **content-addressed filesystem blob area** (`<db>.blobs/sources/<sha256>.eml`; an
  in-memory store uses a temp dir), deduping identical payloads and verifying the
  content hash on read; SQLite keeps only metadata (`message_source`). The blob I/O
  runs off the connection lock. The same raw-source blob is re-parsed on demand for
  inline CID parts and downloadable attachment bytes. This is the content-addressed blob
  foundation durable attachment entities will reuse. Kept for losslessness
  (DKIM/view-source/forward).
- **Text — `MessageBodyStore`** (`put_message_body`/`get_message_body`). The extracted
  body (plain + html) lives in the `message_body` table — small, the reading-view fast
  path (no disk read, no re-parse), and the **search** source. A trigger maintains
  `message_body_fts` (FTS5 over the plain text). Because this index is lease-free and
  sync never touches it, an IMAP re-snapshot cannot wipe it; `search_mail` matches it
  alongside the scope FTS (RRF-fused) and joins to the live `message` rows so stale rows
  for deleted messages drop out, and to `message_body.account` so IMAP keys that
  collide across accounts cannot cross over (`search.md`).
- The fetch-throughs are `engine_sync::fetch_message_body` (text in SQLite → on-disk
  raw → one provider fetch; best-effort caching of both),
  `fetch_inline_parts`, `fetch_message_attachments`, and `fetch_message_attachment`
  (raw blob → one provider fetch if missing; best-effort raw caching). They surface as
  `Engine::message_body`, `message_inline_parts`, `message_attachments`, and
  `message_attachment` (`engine-api.md`). Durable per-attachment blob entities, quota
  eviction, and embeddings/RAG over the indexed text are later slices.
- A host warming the cache in bulk (an offline-first client fetching every body in its
  synced window) plans its work with `Engine::mail_missing_body(accounts, limit)` —
  the list read's ranking (newest first) with the already-warm rows filtered out **in the
  query**, then pulls each result through `Engine::message_body` as usual. The absence
  test belongs in the store because the warm set is the larger half: answered in the
  caller, an already-warm mailbox has every cached key read out and diffed, every pass,
  to conclude there is nothing to do. The warming loop itself (pacing, retry, when to
  run) is host policy, not engine state.

## Re-normalization on a normalizer-version change

The store is a re-derivable cache of **normalized** provider data — how a provider
decodes wire bytes into objects (subject charset, header parsing) and how
`engine-core` projects them. When that logic changes, already-synced objects hold the
*old* normalization and an incremental delta sync will never refresh them (it only
fetches what changed on the server, not what the engine now decodes differently).

`engine_store::NORMALIZER_VERSION` is the marker for that logic. A backend records it
(store-sqlite: a `meta` row) and, **on open**, clears every scope cursor when the stored
value differs — so the next sync re-snapshots and re-normalizes everything. A pre-marker
database (no row) reads as a mismatch and gets exactly that one-time re-sync. Bump
`NORMALIZER_VERSION` whenever a change alters the bytes-to-object mapping in any provider
or in `engine-core` (e.g. the Windows-1252 subject fix); a purely additive change need
not. The cursor clear leaves scope rows and objects in place — the re-snapshot overwrites
and tombstones them — so nothing is orphaned, and the durable outbox is untouched.

The **host-triggered reset** (`Engine::reset`) uses the same primitive: clear the cursors
so the next sync is a full refetch. It is the manual counterpart of the automatic
version-driven clear — a "reset / clean state" action a host exposes, and the escape hatch
if a store is ever suspected stale.

## Local depth narrowing without a provider

The per-sync `window` (`SyncWindow { since }`, above) lets a host **widen or narrow** sync
depth on the next sync without reconnecting. Narrowing is enforced for a reachable account by
clearing the mail cursors and re-syncing: the snapshot under the narrower window carries only
in-window ids, so its reconciliation **tombstones the out-of-window rows**. That path needs the
provider. When the account cannot reach it, the app can store the new depth but cannot force
that re-snapshot — so the engine owns a local cleanup that reproduces the *same* end state
offline: `SqliteStore::prune_account_mail_outside_window` (`Engine::prune_account_mail_outside_window`),
returning a `PruneReport { messages_removed }` (`engine-store`).

- It filters `message.date_utc` — the message's `received_at` falling back to `sent_at`,
  which is exactly the field a provider window maps to (IMAP `SINCE`, JMAP `after` on
  `receivedAt`, Graph `receivedDateTime ge`) — so it tombstones precisely the mail a
  narrower-window snapshot would. Comparison is on the UTC **date** prefix against the window's
  **inclusive** floor date, so mail *on* the floor is kept and only strictly-earlier mail drops.
- **Undated mail is kept.** A message with no `date_utc` is not provably out of window, and a
  prune must never over-delete; a `NULL` date is left in place (an unbounded window is likewise
  a no-op — nothing is outside it).
- It reuses the scope **tombstone** (object + all derived search/thread/occurrence rows), so
  the removed mail leaves nothing orphaned and search/reads reflect it immediately — the same
  cleanup a snapshot reconciliation performs. It runs only over the account's **mail** scopes
  (`SyncScope::search_domain() == Mail`); calendar/contact objects, account metadata, and other
  accounts are untouched.
- Like `forget_account` it is **not lease-gated** — the store's single connection serializes it
  atomically against any in-flight sync — and it **advances no cursor**, so a later delta sync
  resumes unaffected (a delta brings new arrivals only and never re-adds the pruned tail). The
  lease-free body/source caches and the content-addressed raw blobs are left to size-based
  eviction and `VACUUM`, exactly as a normal tombstone and `forget_account` leave them; run
  `vacuum` afterwards to reclaim the freed pages.

## The outbox

Pending ops are durable before any side effect and are claimed with the same
fencing discipline as scopes. The thin inline drivers built on this are
`engine_sync::{submit_mail, edit_mail, create_calendar_event, patch_calendar_event,
delete_calendar_event, put_calendar_document}`. `edit_mail` applies a `MailEdit`
(mark-read/flag, move, or permanent delete) and serializes on the target message key
(`mail:{key}`), recording a plain classified `Failed` on error (no `NeedsConfirmation`: a
mail edit is not post-`DATA`-ambiguous like an SMTP send, and a stale-target `Conflict`
self-corrects after a re-sync). The calendar drivers do the same, and serialize on the
event's **`UID`** (`event:{uid}`) — the cross-system identity, which exists *before* a
create has a provider id and survives a transport that assigns its own (JMAP), so writes to
one event never race on either provider.

- **The payload is the intent, not the rendered bytes.** A calendar patch stores the
  `EventEdit` — which occurrence, and what changed — never the document it produced. That is
  what makes a `Conflict` recoverable: the retry re-applies the edit to a **freshly fetched**
  base. Re-sending bytes built from the copy the server has moved past would silently revert
  somebody else's edit with a write the server happily accepts. (The drainer that will do
  that recovery is issue #60; today a `Conflict` is recorded and surfaced to the caller.)

- **A write does not update the store; a *reconcile* does** (issue #65). The drivers are
  deliberately pure: they record the op, call the provider, record the outcome. They never
  touch the stored object — a write's response is a receipt, not a document, and the store's
  copy must keep coming from the **server** (`caldav.md`). So the read-your-writes step is a
  separate primitive, `engine_sync::reconcile_calendar_events`: the **event-scope delta**,
  which re-delivers the object the server now holds, tombstones a delete, and advances the
  cursor — one round trip, no new provider verb. The `Engine` facade runs it after every
  calendar write (`engine-api.md`), so a host gets read-your-writes by construction while the
  drivers stay usable by a **headless** caller — the #60 drainer has no host `horizon`/zone
  and cannot expand occurrences, which is exactly why the reconcile is not folded into them.
  It can never fail the write: a write that landed but did not reconcile is still a write,
  reported as `Reconciled::{Busy, Failed}` rather than as an error.

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
        T: SyncObject + Serialize + Send + Sync;

    async fn apply_maintenance(
        &self,
        lease: &SyncLease,
        derived: &DerivedWrite,
    ) -> Result<()>;

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()>;
    async fn abandon_sync_leases(&self) -> Result<usize>; // startup recovery only

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
(lease-free object/key inspection, plus `account_scopes` to enumerate an account's
claimed scopes, `scope_objects` to batch-read a scope's objects, and `list_mail` to read
a mailbox list — a window, a conversation, or named messages — without reading the mail
around it) backs the contract suite and the read path.

Supporting types (abbreviated):

- `SyncScope` — enum over `JmapType { account, ty }`, `ImapMailboxList { account }` (the IMAP folder-list container), `ImapMailbox { account, mailbox }`, `DavCollectionList { account }` (the CalDAV/CardDAV collection-list container), `DavCollection { account, collection }`.
- `SyncLease` / `OpLease` — opaque, store-issued; expose fencing token, bound identity, and expiry.
- `Keyed` — one accessor (`provider_key`) for anything a sync carries, whole object or partial, so a carrier can key what it holds without knowing which it is.
- `SyncObject: Keyed` — what a sync pass reports changes to. Names the object's partial form (`Patch`) and how it is persisted (`to_payload`, which mail overrides to write a `MailContent`). `ApplyBatch<'a, T>` and `apply_sync_update` are generic over it.
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
- `abandon_sync_leases` frees held leases without clearing cursors, and fences out
  the abandoned worker by bumping the token.
- Container-before-member apply ordering holds, including under snapshot
  tombstoning. (The store enforces per-scope snapshot tombstoning and keeps
  scopes independent; the cross-scope *apply order* itself is an orchestrator
  invariant, locked in `engine-sync` rather than in the store.)

### A backfill is pinned to its own schema version

A migration's backfill runs against the schema **as of its own step**, and the live write path
moves on. So a backfill writes its own SQL and does not borrow `derived_ops` — sharing the live
upsert means a later migration silently breaks an earlier migration's backfill, which is exactly
how v9's was found broken by v11 (`insert_v9_row` in `backfill.rs`). The migration tests run each
step against a store built only up to the step before it, which is what catches this.
