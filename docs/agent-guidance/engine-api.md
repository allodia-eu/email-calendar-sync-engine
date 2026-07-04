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
  (or `sync_mail_streamed` for live progress); read with `mailboxes` / `messages` /
  `calendars` / `events` and `search_mail` / `search_calendar` (which now also
  matches fetched **body** text); open a message with `message_body` (fetch-on-demand;
  caches the raw bytes on disk and the extracted text in SQLite, so reopen is a fast
  SQLite read and the body becomes searchable), resolve inline CID resources with
  `message_inline_parts`, list ordinary downloadable attachments with
  `message_attachments`, fetch a selected attachment with `message_attachment`; and
  write with `submit_mail` (send) / `edit_mail` (mark-read/flag, move, delete) /
  `write_calendar_event` / `delete_calendar_event` / `pending_op_state`.
  The read
  surface enumerates the account's scopes and filters by `SyncScope::object_kind`, so
  the facade never hard-codes which scopes a provider uses. The return values (e.g.
  `MailSyncReport`, `Vec<Message>`, `Vec<Event>`, `SearchResults`, `SubmitOutcome`) are
  the host's feedback.

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
- **Re-export signature types.** Types that appear in the facade's own signatures
  (`AccountId`, `TimeZoneId`, `Horizon`, the sync reports, `Provider`) are
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
   `ApiError::Sync`. `Engine::write_calendar_event` / `Engine::delete_calendar_event`
   ride the same outbox for calendar mutations — a caller-minted idempotency key plus an
   `EventWrite` (conditional `PUT`) or `EventDeletion` (`DELETE`), returning a
   `CalendarWriteOutcome` / op id; a host builds the create body with
   `provider_caldav::build_event_ical` (the write types are re-exported from
   `engine-api`). A `412` precondition failure surfaces as a `Conflict` (`caldav.md`).
4. **Streaming progress — _done_.** `Engine::sync_mail_streamed` drives
   `engine-sync`'s `sync_mail_streamed`: the email scope commits page by page under one
   lease, reporting `SyncProgress { scope, fetched, total }` to the host's
   `ProgressSink` after each committed page — so a UI shows recent mail and a
   "downloaded Y of X" bar before the sync finishes. Only the final page advances the
   cursor (a mid-stream crash re-runs the pass idempotently). A closure is a sink via
   the blanket `ProgressSink for Fn(SyncProgress)` impl.
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
reads back `None`. A streamed `sync_mail_streamed` with a closure sink then asserts
one progress event lands with `fetched == total == 2`. Run the standard gate (`AGENTS.md`):
`cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D
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
