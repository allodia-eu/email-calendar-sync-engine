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
  (and, as slices land, search and writes). The return values (e.g.
  `MailSyncReport`) are the host's feedback.

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
  resolution — enough for lease liveness). Keep new real-world I/O seams (clock,
  later: network policy, blob roots) on this side of the boundary.
- **Generic over `Provider`.** `sync_*` take `&impl Provider`, so the facade is
  provider-agnostic and a host passes a `provider-jmap` / `provider-imap` /
  `provider-caldav` adapter. (The `engine-sync` free functions are generic over
  `P: Provider`; `dyn Provider` does not implement `Provider`, so a host holding a
  `Box<dyn Provider>` cannot call these yet. If/when a binding needs dynamic
  dispatch across providers, add a blanket `impl Provider for Box<dyn Provider>` in
  `engine-provider` as its own slice — do not special-case it in `engine-api`.)
- **One logical writer.** An `Engine` stamps a fixed `WorkerId` ("engine-api") on
  its leases; the fencing token (not the id) serializes a suspended-then-resumed
  worker against itself (`store-and-sync.md`). Distinct-per-device worker
  identities for a genuinely multi-writer account are a later, host-configured
  slice. The lease TTL is a generous safety bound (`LEASE_TTL`), not a deadline —
  the sync loop re-claims and recomputes on `StaleLease`.
- **Re-export signature types.** Types that appear in the facade's own signatures
  (`AccountId`, `TimeZoneId`, `Horizon`, the sync reports, `Provider`) are
  re-exported so a host depends on `engine-api` alone. The concrete provider still
  comes from the adapter crate.

## Slice plan

Step 6 lands in small, tested slices. Order and status:

1. **Lifecycle + provider-driven sync — _done_.** `open`/`open_in_memory`,
   `sync_mail`, `sync_calendar`, `SystemClock`, and `ApiError`.
2. **Per-account search.** Add a store-read primitive to enumerate an account's
   scopes — the `sync_scope` table already stores `account`, and a `scope_key` is
   just `serde_json` of a `SyncScope`, so this is a cheap, provider-agnostic
   `SELECT` — give it a contract test in `engine-store`, then expose
   `Engine::search_mail` / `search_calendar` (parse DSL → run over the account's
   scopes → `SearchResults` with coverage). Do **not** hard-code JMAP scopes the
   way the CLI fixture harness does; enumerate them.
3. **Writes / outbox.** Surface `submit_mail` and pending-op inspection over the
   `engine-sync` outbox path.
4. **Streaming progress.** Expose `sync_mail_streamed` + a `ProgressSink` the host
   can observe for "downloaded Y of X" UI.
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
- **Errors wrap, never restring.** `ApiError` variants carry the underlying engine
  error unchanged so its `source()` chain (provider failure class, store backend
  detail) stays inspectable.
- **Real clock stays whole-second and forward-only.** Lease ordering depends on
  `now()` never moving backwards across a second boundary; do not "optimize" it
  into something that can.

## Verification

The crate's deterministic tests cover it without the Stalwart harness: an
end-to-end `tests/sync.rs` opens an `Engine` and syncs mail+calendar through a fake
`Provider` (the same way a host would), asserts the sync reports, and checks that a
provider failure surfaces as `ApiError::Sync` and a bad path as `ApiError::Store`.
Run the standard gate (`AGENTS.md`): `cargo fmt --check`, `cargo clippy --workspace
--all-targets --all-features -- -D warnings`, `cargo test --workspace
--all-features`, `cargo doc`. `engine-api`'s own lines are 100%-covered by these
tests (no live provider needed).

[`Engine`]: ../../crates/engine-api/src/engine.rs
[`SystemClock`]: ../../crates/engine-api/src/clock.rs
[`SqliteStore`]: ../../crates/store-sqlite/src/lib.rs
