# email-calendar-sync-engine

> ⚠️ **Work in progress — early design draft.** No working code yet, and the
> design may still change. This repository currently holds the architecture and
> domain-model specification, not an implementation.

A standalone **Rust engine for personal information management (PIM)**:
local-first mail and calendar sync, search, indexing, and durable writes,
designed to be embedded by native apps, command-line tools, local daemons, and
server-side adapters.

The engine is **provider-agnostic** — it speaks modern and legacy protocols
behind one normalized model — and keeps a local, encrypted source of truth so
the host stays useful offline. Mail and calendar are the focus (contacts later).

## Status

Design stage. The architecture and domain model live under
[`docs/agent-guidance/`](docs/agent-guidance/); implementation has not started,
so treat every interface and crate below as provisional.

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
