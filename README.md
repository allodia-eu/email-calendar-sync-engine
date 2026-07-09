# email-calendar-sync-engine

[![CI](https://github.com/allodia-eu/email-calendar-sync-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/allodia-eu/email-calendar-sync-engine/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/allodia-eu/email-calendar-sync-engine/graph/badge.svg?token=43R3X4R59A)](https://codecov.io/gh/allodia-eu/email-calendar-sync-engine)
![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)

A standalone **Rust engine for personal information management (PIM)**: local-first mail and calendar sync, search, indexing, and durable writes. Designed to be embedded by native apps, command-line tools, local daemons, and server-side adapters.

The engine is **provider-agnostic**: it speaks modern and legacy protocols behind one normalized model, and keeps a local, encrypted source of truth so the host stays useful offline. Mail and calendar are the focus (contacts later).

> **Status:** The core domain model, sync orchestration, SQLite store, search/index, MIME body extraction, shared TLS stack, and several provider adapters are implemented and tested. The project is still in early product development, so public APIs may evolve as more hosts and language bindings are integrated.

## What the engine gives a host

- **One normalized model** for mail and calendar objects, regardless of whether the server speaks JMAP, IMAP, CalDAV, or Microsoft Graph.
- **Local-first sync** into an encrypted SQLite store with full-text search, so reads and search work offline.
- **Durable writes** (send, mark-read, move, delete, calendar event create/update/delete) through a crash-safe outbox.
- **Streaming sync** that commits chunks as they arrive, so a UI can render recent mail and progress before a full mailbox finishes.
- **Provider-native raw payloads preserved** (MIME, iCalendar, JSCalendar) for lossless re-parsing and writes.
- **Multi-account** by design, with a shared TLS trust policy per host.

## Implemented providers

| Provider | Crate | Protocols | Standards & extensions | Mail | Calendar | Push | Writes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **JMAP** | `provider-jmap` | JMAP Core, JMAP Mail, JMAP Calendars | RFC 8620, RFC 8621, RFC 8984 (JSCalendar), RFC 8620 §7.3 EventSource push | read, write, submit | read | EventSource | mail |
| **IMAP + SMTP** | `provider-imap` | IMAP4rev2, SMTP | RFC 9051, RFC 7162 (CONDSTORE/QRESYNC), RFC 2177 (IDLE), RFC 6154 (SPECIAL-USE), RFC 6851 (MOVE), RFC 4315 (UIDPLUS), RFC 5321/5322, RFC 2047 | read, write | — | IDLE | mail (SMTP submit optional) |
| **CalDAV** | `provider-caldav` | CalDAV / WebDAV, iCalendar | RFC 4791, RFC 5545, RFC 6578 (sync-collection), RFC 6764 (discovery), RFC 6638 (scheduling), RFC 6047 (iMIP) | — | read, write | — | calendar + iMIP RSVP |
| **Microsoft Graph** | `provider-graph` | Microsoft Graph v1.0 | Graph mail `delta` API, `Prefer: IdType="ImmutableId"` | read | — | — | — |

See [`docs/providers.md`](docs/providers.md) for the full provider matrix, RFC/standard details, and per-provider connection examples.

## Quickstart

Add `engine-api` and the provider crates you need to your `Cargo.toml`:

```toml
[dependencies]
engine-api = { path = "crates/engine-api" }
provider-jmap = { path = "crates/provider-jmap" }
provider-imap = { path = "crates/provider-imap" }
engine-tls = { path = "crates/engine-tls" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### Open the engine

```rust
use engine_api::{AccountId, Engine};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::open("engine.sqlite")?;
    let account = AccountId::try_from("alice@example.com")?;
    // ... connect a provider and sync
    Ok(())
}
```

### Connect to a JMAP server

```rust
use engine_tls::TlsClientConfig;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};

let tls = TlsClientConfig::bundled(); // or bundled_and_system(), system_only(), etc.
let config = JmapConfig::new(
    "https://jmap.example.com",
    Credentials::basic("alice@example.com", "app-password"),
)
.with_tls(tls);

let provider = JmapProvider::connect(config).await?;
```

### Connect to an IMAP server

```rust
use engine_core::ids::MailboxId;
use engine_tls::TlsClientConfig;
use provider_imap::{ImapConfig, ImapProvider};

let tls = TlsClientConfig::bundled();
let config = ImapConfig::new(
    "imap.example.com:993",
    "imap.example.com", // SNI / certificate name
    "alice@example.com",
    "app-password",
);

let provider = ImapProvider::connect(
    &config,
    tls.connector(),
    MailboxId::try_from("INBOX")?,
)
.await?;
```

### Sync mail

```rust
let report = engine.sync_mail(&provider, &account).await?;
println!(
    "mailboxes: {}, messages: {}",
    report.mailboxes.upserted, report.email.upserted
);
```

### Read and search

```rust
let mailboxes = engine.mailboxes(&account).await?;
let messages = engine.messages(&account).await?;
let search = engine
    .search_mail(&account, "subject:report from:alice", 10)
    .await?;
```

### Send mail (JMAP example)

```rust
use engine_api::{Draft, EmailAddress, MessageIdHeader};

let draft = Draft::new(
    MessageIdHeader::new("unique-id@example.com")?,
    EmailAddress::new("alice@example.com"),
    vec![EmailAddress::new("bob@example.com")],
    "Hello",
    "This is the message body.",
);
let outcome = engine.submit_mail(&provider, &account, &draft).await?;
```

## How the pieces fit

```text
Host apps / mobile / desktop / CLI / server
    │
    ▼
engine-api (stable Rust facade)
    │
    ▼
Engine
 ├─ sync ──► provider adapters ──► JMAP / IMAP / CalDAV / Graph
 ├─ outbox ──► durable writes
 ├─ search / index ──► SQLite FTS
 └─ store ──► SQLite (encrypted)
```

## Workspace layout

```text
.
├── Cargo.toml                    # virtual workspace + shared lints/deps
├── crates/
│   ├── engine-api/               # Host-facing facade
│   ├── engine-core/              # Domain model, ids, pure logic
│   ├── engine-sync/              # Sync orchestration and outbox
│   ├── engine-provider/          # Provider trait and shared contracts
│   ├── engine-store/             # Store trait
│   ├── store-sqlite/             # SQLite implementation
│   ├── engine-search/            # Query DSL and executor
│   ├── engine-recurrence/        # Recurrence expansion
│   ├── engine-mime/              # MIME / RFC 5322 body extraction
│   ├── engine-tls/               # Shared TLS trust policy
│   ├── provider-jmap/            # JMAP adapter
│   ├── provider-imap/            # IMAP + SMTP adapter
│   ├── provider-caldav/          # CalDAV adapter
│   ├── provider-graph/           # Microsoft Graph adapter
│   ├── engine-cli/               # Headless debugging / fixture harness
│   └── stalwart-harness/         # Docker-based protocol test harness
└── docs/
    ├── providers.md              # User-facing provider guide
    └── agent-guidance/           # Architecture and modeling specs
```

## Building and testing

See [`BUILDING.md`](BUILDING.md) for prerequisites. In short:

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
```

The full CI gate (format on nightly, clippy, build, test, docs) is documented in [`AGENTS.md`](AGENTS.md).

## Design and agent documentation

- [`docs/agent-guidance/north-star.md`](docs/agent-guidance/north-star.md) — product goal and architecture
- [`docs/providers.md`](docs/providers.md) — provider capabilities, RFCs, and connection examples
- [`docs/agent-guidance/engine-api.md`](docs/agent-guidance/engine-api.md) — host facade design
- [`docs/agent-guidance/providers.md`](docs/agent-guidance/providers.md) — provider contract
- [`docs/agent-guidance/store-and-sync.md`](docs/agent-guidance/store-and-sync.md) — store and sync model
- [`docs/agent-guidance/tls.md`](docs/agent-guidance/tls.md) — TLS trust policy
- [`docs/agent-guidance/jmap.md`](docs/agent-guidance/jmap.md), [`imap-smtp.md`](docs/agent-guidance/imap-smtp.md), [`caldav.md`](docs/agent-guidance/caldav.md), [`graph.md`](docs/agent-guidance/graph.md) — per-provider deep dives

## License

Not yet decided.
