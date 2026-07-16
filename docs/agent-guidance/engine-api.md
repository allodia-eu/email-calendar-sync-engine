# `engine-api` — the host facade

`engine-api` is the stable, host-facing entry point to the engine (`north-star.md`:
*"Host-facing APIs live behind `engine-api`."*). It is the **one composition
point**: instead of wiring `engine-store`, `engine-sync`, the providers, a search
layer, and a clock together, every host — mobile (UniFFI), desktop/daemon (the C
ABI), the CLI, and server adapters — drives the engine through this crate.

This doc is authoritative for the facade's shape and the order its slices land.
Read it before touching `engine-api` or adding a binding/reference-host seam.

## What it is

- An [`Engine`] owns **one durable [`SqliteStore`]** driven by a host wall clock
  ([`SystemClock`]), and exposes high-level operations over it.
- Hosts call `Engine::open` / `open_in_memory`, then `sync_mail` / `sync_calendar`
  (or `sync_mail_streamed` — or the per-folder `sync_mailbox_list` +
  `sync_folder_email_streamed` — for live progress and change events); read with
  `mailboxes` / `messages` /
  `calendars` / `events` and `search_mail` / `search_calendar` (which now also
  matches fetched **body** text); open a message with `message_body` (fetch-on-demand;
  caches the raw bytes on disk and the extracted text in SQLite, so reopen is a fast
  SQLite read and the body becomes searchable), plan a bulk body-warming pass with
  `messages_missing_body` (the newest synced messages whose body text is not yet
  cached — a host feeds each through `message_body` to make its window readable
  offline), resolve inline CID resources with
  `message_inline_parts`, list ordinary downloadable attachments with
  `message_attachments`, fetch a selected attachment with `message_attachment`; and
  write with `submit_mail` (send) / `edit_mail` (mark-read/flag, move, delete) /
  `create_calendar_event` / `patch_calendar_event` / `delete_calendar_event`
  (+ `put_calendar_document`, the iMIP RSVP escape hatch) / `pending_op_state`.
- **A calendar grid reads `occurrences_in`, not `events`.** `events` returns the
  projected envelope — a recurring series is one object, at its series start — so a host
  that lays *that* out shows a weekly meeting in exactly one week. `occurrences_in(account,
  window)` returns the materialized instances overlapping a half-open UTC window, each
  pointing back at its master for the title/participants. Pair it with `to_local` /
  `day_bounds_utc` (the only UTC→local direction the engine offers, so a host never
  bundles a second tzdb) to build the window and place a row in a day column.
- **Widen the horizon with `expand_horizon`; a re-sync will not.** Sync expands only what
  its delta *changed*, so reading a window no sync ever materialized returns empty —
  permanently, no matter how often the host re-syncs. `expand_horizon` re-derives the
  stored events over a new window with no network, and is also the path for a display-zone
  or tzdata change. Both it and `sync_calendar` report the events they could **not**
  expand (`unexpandable`): those materialize zero occurrences and so render nowhere, and
  the host is expected to surface that rather than lose them silently.
  The read
  surface enumerates the account's scopes and filters by `SyncScope::object_kind`, so
  the facade never hard-codes which scopes a provider uses. The return values (e.g.
  `MailSyncReport`, `Vec<Message>`, `Vec<Event>`, `SearchResults`, `SubmitOutcome`) are
  the host's feedback.
- Providers are **host-constructed**, not owned by `Engine`: the host builds each
  provider — passing one shared `engine_tls::TlsClientConfig` for the account
  (`tls.md`) — and hands it to `sync_*`. Exposing the `TlsPolicy` over the bindings
  is a later slice.

## What it is not

- It is **not** a second home for domain logic. Normalization, projection,
  recurrence expansion, the store contract, and sync orchestration stay in their
  crates; `engine-api` only composes them.
- It is **not** provider-aware. It never switches on protocol or names a concrete
  provider — see the provider-agnostic invariant below.

## Key decisions

- **Concrete store, not `dyn Store`.** SQLite is the engine's first store, and the
  search and other conveniences live on `SqliteStore` (inherent methods), not on
  the `engine_store::Store` trait. The facade therefore holds a concrete
  `SqliteStore<SystemClock>`. Other stores are host adapters; if a second store
  ever ships, that is the point to introduce a store-selection seam, not before.
- **The wall clock lives here.** `engine-store` ships only `ManualClock` for
  deterministic tests and never reads wall-clock time itself; the engine's time
  source stays one injected seam. `engine-api` supplies the real one
  (`SystemClock`, built from `time::OffsetDateTime::now_utc()`, whole-second
  resolution — enough for lease liveness; it is a wall clock, so cross-step
  ordering rests on the TTL + `StaleLease` reclaim, not on the clock). It is
  crate-internal (`pub(crate)`) for now — nothing public accepts a clock — and
  becomes public when a clock-injection constructor lands (see deferred seams
  below). Keep new real-world I/O seams (clock, later: network policy, blob roots)
  on this side of the boundary.
- **Generic over `Provider`.** `sync_*` take `&impl Provider`, so the facade is
  provider-agnostic and a host passes a `provider-jmap` / `provider-imap` /
  `provider-caldav` adapter. (The `engine-sync` free functions are generic over
  `P: Provider`. A host that picks a concrete adapter at runtime can hold a
  `Box<dyn Provider>` and still call them: `engine-provider` provides a blanket
  `impl<P: Provider + ?Sized> Provider for Box<P>` that delegates every method to the
  box's contents — kept there, not special-cased in `engine-api`.)
- **Host-config is hardcoded in this slice, by design (deferred seams).** An
  `Engine` stamps a fixed `WorkerId` (`"engine-api"`), uses a fixed `LEASE_TTL`
  (5 min — a generous safety bound, not a deadline; the sync loop re-claims and
  recomputes on `StaleLease`), and constructs its own `SystemClock`. The durable
  docs describe all three as host-controlled seams — host-assigned worker identity,
  a *"TTL (host-tunable via the injected clock)"* (`store-and-sync.md`), and an
  *"injectable clock/time source"* (`north-star.md`) — and the engine layers below
  honor them; the **facade just does not expose them yet**. Host-supplied worker id
  (for multi-device lease attribution), host-tunable TTL, and clock injection (for
  deterministic facade tests) are deferred to a later slice; threading them through
  `open()`/`sync_*` then is an additive change. Until then, fencing tokens (not the
  worker id) still serialize writers correctly.
- **Concurrent same-scope syncs resolve to `Busy`, not corruption.** `Engine` is
  `Send + Sync`; share one as `Arc<Engine>`. Two syncs of *different* scopes run in
  parallel, but two of the *same* `(account, scope)` cannot both hold its lease: the
  store returns the retryable `ScopeHeld`, the sync loop surfaces it (it recovers
  only `StaleLease`), and the facade maps it to `ApiError::Busy` — a distinct,
  retryable signal separate from `ApiError::Sync`. The facade does **not** itself
  queue or auto-retry; a host serializes per account or retries on `Busy`. If a
  future slice wants transparent serialization, add a per-account async lock in the
  facade — do not widen `run_scope` to swallow `ScopeHeld`.
- **Abrupt process recovery is explicit.** A host that knows prior workers for the
  store are gone after process death can call `Engine::abandon_sync_leases` once at
  startup. It clears held scope leases and bumps their fencing tokens while
  preserving cursors, so a cold backfill resumes from its last committed checkpoint
  immediately instead of waiting for the fixed `LEASE_TTL` or clearing state. This
  is not a normal `Busy` recovery path for live in-process contention.
- **Re-export signature types.** Types that appear in the facade's own signatures
  (`AccountId`, `TimeZoneId`, `Horizon`, the sync reports, `Provider`, and the
  streaming vocabulary — `StreamTuning`, `SyncObserver`, `SyncCommit`, `IgnoreCommits`,
  `AccountProgress`, `ProgressSnapshot`, `SyncScope`, `SyncWindow`, `CalendarDate`) are
  re-exported so a host depends on `engine-api` alone. The concrete provider still
  comes from the adapter crate.
- **Display-side timezone resolution.** `resolve_instant` / `resolve_instant_in` /
  `is_supported_zone` (with `ExpandError`) are re-exported from `engine-recurrence`
  so a host can resolve a stored event's start to its absolute UTC instant for
  local-zone display (`resolve_instant`), get a total-order sort key for a
  mixed-kind agenda in a chosen display zone (`resolve_instant_in`), and validate a
  picked/device zone before adopting it (`is_supported_zone`) — without depending on
  `engine-recurrence` or bundling tzdata itself (`calendar-semantics.md`).

## Slice plan

Step 6 lands in small, tested slices. Order and status:

1. **Lifecycle + provider-driven sync — _done_.** `open`/`open_in_memory`,
   `sync_mail`, `sync_calendar`, `SystemClock`, and `ApiError`.
2. **Per-account search — _done_.** `StoreRead::account_scopes(account)` enumerates
   an account's scopes (a `SELECT … WHERE account = ?` over `sync_scope`, each JSON
   `scope_key` decoded back to a `SyncScope`; contract-tested in `engine-store`, so
   both the in-memory store and `store-sqlite` satisfy it). `Engine::search_mail` /
   `search_calendar` parse the DSL, filter the account's scopes to the queried
   domain via `SyncScope::search_domain` (so the facade never hard-codes a
   provider's scopes nor branches on protocol), and run them through the store's
   executor — returning `SearchResults` with coverage. A malformed query string is
   `ApiError::Query`.
3. **Writes / outbox — _done_.** `Engine::submit_mail` drives `engine-sync`'s outbox
   `submit_mail` (durable op → claim → provider send → record), returning a
   `SubmitOutcome` (sent key, `Message-ID`, op id); a failed send is recorded
   `Failed` / `NeedsConfirmation` *before* surfacing as `ApiError::Sync`, so the
   outbox never blind-retries. `Engine::pending_op_state` exposes
   `StoreRead::pending_op_state` for polling an op's lifecycle (e.g. confirming an
   ambiguous send). `Engine::edit_mail` rides the same outbox for mail mutations —
   it takes a caller-minted idempotency key and a `MailEdit` (mark-read/flag, move,
   or permanent delete) and returns a `MailEditOutcome` (resolved key + op id); a
   failure (e.g. a stale-target `Conflict`) is recorded `Failed` before surfacing as
   `ApiError::Sync`. `Engine::create_calendar_event` / `patch_calendar_event` /
   `delete_calendar_event` ride the same outbox for calendar mutations — a caller-minted
   idempotency key plus an `EventDraft` (the event you want), or the event **as you read
   it** plus a `PatchTarget` + `EventPatch` (what changed, and on which occurrence), or an
   `EventDeletion` — returning a `CalendarWrite` / `CalendarDelete`. These carry **intent**: the host never assembles
   iCalendar, mints an href, or touches an `ETag`, and the same call drives CalDAV and JMAP
   (`providers.md`). The write types are re-exported from `engine-api`.
   - **Read `Capabilities::calendar_write_guard()` before writing.** `WriteGuard::Enforced`
     (CalDAV) means a stale edit is refused — a `412` surfaces as a `Conflict`, to be
     recovered by re-syncing and re-applying, never a blind retry. `WriteGuard::Absent`
     (JMAP) means the transport **cannot** refuse one: a stale edit silently wins, so a
     successful write does not imply no concurrent edit was lost, and a host that cares must
     detect it itself (`jmap.md`).
   - **A calendar write reconciles the store before it returns** (issue #65). A write's
     response is a *receipt*, not a document (a CalDAV `PUT` answers with an `ETag` and no
     body; a JMAP `/set` with an id and no object), so the driver alone would leave the row
     holding the **pre-write** projection, `raw_ical` and revision. Each facade write
     therefore runs `engine_sync::reconcile_calendar_events` — an **event-scope delta**, one
     round trip, the same primitive a sync reads through — the moment the write lands. The
     store then holds what the **server** holds, a delete is tombstoned locally, and an edit
     that moved the event moves its occurrence rows. That is what makes "edit, re-read, edit
     again" work: the second edit's guard is the revision the *server* reported, not the
     superseded one it wrote over. Proven live against Stalwart (CalDAV + JMAP) and SabreDAV.
     - **A write is never told what the UI is showing.** The reconcile re-expands over the
       window the *store* holds (`ExpansionWindow`), so the write methods take no `horizon`
       or `host_zone`, and a write can neither widen nor narrow what the host has expanded.
       `Engine::expand_horizon` owns the window; see `store-and-sync.md`.
     - **Never store our own bytes instead.** The reconcile must re-read from the server:
       Stalwart *reserializes* what it stores, so an optimistic local copy would put a
       `RawIcal` in the store the server does not have — and would **mask a server that
       silently dropped a property** (`caldav.md`). Body and revision also cannot move
       independently: a row claiming a revision whose bytes it does not hold lets a host
       patch a stale body under a valid guard and silently revert its own edit.
     - **A write that did not reconcile is still a write.** The reconcile is a *local* step
       after a write the server already accepted, so it can never fail the write: it is
       reported as `Reconciled::{Applied, Busy, Failed}` on the outcome, never as an error.
       `Busy` means a concurrent sync holds the event scope. Recover by re-reading
       (`Engine::reconcile_calendar_events`, also the batch path for a host driving the
       low-level `engine_sync` drivers itself) — **never** by re-issuing the write.
4. **Streaming sync — _done_.** `Engine::sync_mail_streamed(provider, account, tuning,
   observer)` drives `engine-sync`'s `sync_mail_streamed`: the email scope commits
   **chunk by chunk** under one lease, reporting a `SyncCommit { scope, fetched, total,
   upserted, removed }` to the host's `SyncObserver` after each committed chunk — so a
   UI shows recent mail and a "downloaded Y of X" bar before the sync finishes **and**
   splices its list from the exact `upserted`/`removed` rows without re-querying the
   mailbox. An additive pass (cold backfill or delta) checkpoints the cursor per chunk,
   so a mid-stream crash resumes where it stopped; a reconcile re-snapshot holds the
   cursor until its tombstoning final chunk (`store-and-sync.md`). `StreamTuning` sets
   the per-sync depth `window` and decouples the fetch batch (round trips) from the
   chunk size (commit granularity). For a **concurrent per-folder fan-out** (IMAP/Graph)
   `Engine::sync_mailbox_list` syncs the folder list once and
   `Engine::sync_folder_email_streamed` streams each folder's mail in parallel, all
   reporting into one observer — e.g. `AccountProgress`, which folds per-folder commits
   into a single account-level "downloaded Y of X". A closure is a `SyncObserver` via
   the blanket impl, and `IgnoreCommits` is the no-op sink.
5. **Bindings.** `bindings-uniffi` (Kotlin/Swift) and `bindings-ffi-c` (C ABI)
   over `engine-api`. These need `unsafe`/codegen, so they override the workspace
   `unsafe_code = "forbid"` lint locally (isolated + documented, per `AGENTS.md`),
   and they pick concrete provider/clock types — `engine-api` stays idiomatic Rust.

When a slice migrates the CLI onto the facade, reconcile `engine-cli`'s docs (its
lib already anticipates *"When `engine-api` lands, the CLI will consume that stable
facade"*).

## Invariants for the next agent

- **Keep it provider-agnostic.** No protocol branching, no naming a concrete
  provider crate in a dependency or signature. New provider behavior belongs in a
  provider crate behind the `Provider` trait.
- **Keep it a thin composition.** If a method grows real logic, that logic
  probably belongs in `engine-sync`/`engine-search`/`engine-core` with a test
  there; the facade just calls it.
- **Errors wrap, never restring.** `ApiError::Store`/`Sync` carry the underlying
  engine error unchanged so its `source()` chain (provider failure class, store
  backend detail) stays inspectable. The one deliberate exception is `ScopeHeld`,
  which `map_sync_error` classifies as `ApiError::Busy` (a retryable race, not a
  failure) — classification, not restringing. Add similar classifications there if
  another error class deserves a distinct host signal.
- **The clock is a wall clock, not monotonic.** `now()` is whole-second and can
  step backward (NTP); do not write code or tests that assume monotonic `now()`.
  Lease safety across a step rests on the TTL + `StaleLease` reclaim in the sync
  loop, not on the clock.

## Verification

The crate's deterministic tests cover it without the Stalwart harness: an
end-to-end `tests/sync.rs` opens an `Engine` and syncs mail+calendar through a
**cursor-aware** fake `Provider` (snapshot first, delta after), the same way a host
would. From the returned reports it asserts: a first snapshot upserts; a resync
after reopening a file-backed store is an *empty delta* (proving the cursor — and
data — persisted, since a lost store would re-snapshot and upsert); a delta that
drops a key tombstones it; a provider failure surfaces as `ApiError::Sync` and a
bad path as `ApiError::Store`; and two concurrent syncs of one scope resolve to
`ApiError::Busy` (a `tokio::sync::oneshot` gate holds one sync's lease while the
other races, deterministically — no timing). The same file's search tests then
exercise per-account search over the synced data: a DSL query finds the matching
mail/event with complete coverage, a malformed query is `ApiError::Query`, and an
unsynced account returns an empty answer. A `SubmittingProvider` then exercises the
outbox facade: a successful `submit_mail` commits the op `Succeeded` (read back via
`pending_op_state`), a failed send surfaces as `ApiError::Sync`, and an unknown op id
reads back `None`. A streamed `sync_mail_streamed` with a closure observer then asserts
one `SyncCommit` lands with `fetched == total == 2`. Run the standard gate (`AGENTS.md`):
`cargo +nightly fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D
warnings`, `cargo test --workspace --all-features`, `cargo doc`. `engine-api`'s own
lines are 100%-covered by these tests (no live provider needed).

The fake `Provider` and object builders in `tests/sync.rs` are a third copy of a
pattern `engine-sync` and `engine-provider` also hand-roll as crate-private test
code. Promoting one shared fake + builders behind a `test-support` feature/module
(so the `Provider` trait has a single fake to update) is a worthwhile follow-up,
deferred here to avoid refactoring three crates' tests in this slice.

[`Engine`]: ../../crates/engine-api/src/engine.rs
[`SystemClock`]: ../../crates/engine-api/src/clock.rs
[`SqliteStore`]: ../../crates/store-sqlite/src/lib.rs
