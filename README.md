# email-calendar-sync-engine

> ⚠️ **Work in progress — early implementation.** The RFC-backed domain model
> (`engine-core`) is implemented and tested; the remaining crates are still
> design-stage and may change. Treat every interface as provisional.

A standalone **Rust engine for personal information management (PIM)**:
local-first mail and calendar sync, search, indexing, and durable writes,
designed to be embedded by native apps, command-line tools, local daemons, and
server-side adapters.

The engine is **provider-agnostic** — it speaks modern and legacy protocols
behind one normalized model — and keeps a local, encrypted source of truth so
the host stays useful offline. Mail and calendar are the focus (contacts later).

## Status

Build order **step 1** is done: the workspace skeleton, strict lint/test policy,
and the RFC-backed domain model in [`crates/engine-core/`](crates/engine-core/) —
identities, the time model, normalized mail and calendar types, scheduling, and
the cross-crate sync/search/write contracts — are implemented and tested
(I/O-free and async-free, per the north star). The remaining crates in the map
below have not been built yet, so treat their interfaces as provisional. The
architecture lives under [`docs/agent-guidance/`](docs/agent-guidance/).

## How the pieces fit (envisioned)

```
  Host apps · mobile · desktop · CLI · server
      │  (own UI, accounts, notifications, rendering)
      │
      ▼  bindings: UniFFI for Kotlin/Swift · C ABI for desktop/daemons
  ┌───────────────────────────────────────┐
  │  engine-api — the stable API we expose │
  └───────────────────────────────────────┘
      │
      ▼
  Engine
   ├─ sync ──────────► provider adapters ──► remote mail / calendar servers
   │                   JMAP · IMAP · SMTP · CalDAV/CardDAV  (Gmail · Graph: later)
   ├─ outbox ────────► durable, crash-safe writes (sent out through the adapters)
   ├─ search / index ─► query the local store
   └─ store ─────────► SQLite, encrypted, with full-text search   ← local source of truth

  Shared, with no I/O of their own:
   • engine-core     — the normalized domain model + cross-crate contracts
   • crypto-keystore — credentials & encryption keys, never stored in the database
```

In words:

- **Hosts** own the UI, account onboarding, OS permissions, notifications, and
  rendering. They never talk to mail/calendar servers directly — they go through
  the engine.
- **`engine-api`** is the single stable, host-facing API. Mobile reaches it via
  **UniFFI** (Kotlin/Swift); desktop and daemon hosts via a **C ABI**.
- Internally the engine **syncs** each account through **provider adapters**,
  normalizes everything into the shared **core model**, and commits to a local
  **SQLite store** (encrypted, with full-text search). **Search/index** read that
  local store. Outgoing changes flow through a durable **outbox** so they survive
  crashes and network loss.
- **`engine-core`** holds the pure domain model and the contracts the other
  crates share; **`crypto-keystore`** holds secrets, which never enter the
  database.

## What we expose

- **`engine-api`** — the stable Rust facade hosts build against.
- **Bindings** — `bindings-uniffi` (Kotlin/Swift, for Android/iOS) and
  `bindings-ffi-c` (C ABI, for desktop/daemon hosts).
- A **headless CLI** for fixtures, sync debugging, search, and outbox replay.

## Building

See [`BUILDING.md`](BUILDING.md) for prerequisites and the build/test/lint/docs/
coverage commands. In short:

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs formatting,
clippy (`-D warnings`), tests, docs, and code coverage on every push and PR.

## Design docs

The design is specified in [`docs/agent-guidance/`](docs/agent-guidance/) — start
with the north star:

- [`north-star.md`](docs/agent-guidance/north-star.md) — product goal, architecture, invariants.
- [`modeling.md`](docs/agent-guidance/modeling.md) — domain-model rules and required fixtures.
- [`providers.md`](docs/agent-guidance/providers.md) — provider/adapter contract.
- [`store-and-sync.md`](docs/agent-guidance/store-and-sync.md) — store, lease, and sync concurrency contract.
- [`search-coverage.md`](docs/agent-guidance/search-coverage.md) — how search reports completeness.
- [`calendar-semantics.md`](docs/agent-guidance/calendar-semantics.md) — timezones, scheduling, recurrence boundaries.
- [`rust.md`](docs/agent-guidance/rust.md) — Rust API and style standards.

## License

Not yet decided.
