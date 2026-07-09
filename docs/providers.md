# Provider guide

This guide lists the protocols the engine speaks, the standards and extensions each provider implements, and how to connect and sync them. For the internal provider contract (trait design, scopes, streaming, errors), see [`docs/agent-guidance/providers.md`](docs/agent-guidance/providers.md).

## How providers are exposed

Each provider crate implements the `engine_provider::Provider` trait. The host constructs the adapter, passes it to `engine-api` methods, and the engine never switches on protocol. A provider advertises what it can do through `ConnectionInfo::capabilities`, and reports what the transport negotiated (TLS version for IMAP, HTTP version for JMAP/CalDAV/Graph) through the same object.

All providers share one TLS trust policy via `engine_tls::TlsClientConfig`. See [`docs/agent-guidance/tls.md`](docs/agent-guidance/tls.md) for the trust model and platform guidance.

## Observing the connect phase

`ConnectionInfo` describes a connection that already exists. To watch one being
established — the well-known redirects a provider follows itself, the TLS handshake,
authentication, the endpoint discovery settles on — attach a `ConnectObserver` to the
adapter's config. Any `Fn(&ConnectStep<'_>)` will do:

```rust
use std::sync::Arc;
use engine_provider::ConnectStep;

let observer = Arc::new(|step: &ConnectStep<'_>| match step {
    ConnectStep::Redirected { from, to, .. } => tracing::info!("resolved {from} -> {to}"),
    ConnectStep::TlsEstablished(version) => tracing::info!("TLS {version:?}"),
    ConnectStep::Authenticated => tracing::info!("authenticated"),
    ConnectStep::Discovered { endpoint, .. } => tracing::info!("endpoint {endpoint}"),
    _ => {}
});

let provider = JmapProvider::connect(config.with_connect_observer(observer)).await?;
```

The observer lives on the config, so rebuilding a provider from it after a dropped
session observes the redial too. What each adapter reports differs, for the same
reasons `ConnectionInfo`'s version fields do:

| Provider | Steps |
| --- | --- |
| **JMAP** | `Redirected` per well-known hop, `Authenticated`, `Discovered` (the `apiUrl`) |
| **IMAP** | `TlsEstablished`, `Authenticated` |
| **CalDAV** | `Redirected` per hop, `Discovered` (the calendar home) |
| **Microsoft Graph** | none — `GraphClient::connect` performs no I/O |

Only IMAP can report a TLS version (it drives rustls directly); only JMAP and CalDAV
follow redirects themselves. URLs reaching an observer have any `user:pw@` credentials
stripped, so a step is always safe to log.

The engine reports *steps*, not a connection *state*. A `Disconnected`/`Connecting`/
`Connected` machine belongs to the host: it is the layer that knows a call just failed
with `FailureClass::Retryable` and that a reconnect is in flight. The engine supplies
the inputs — the `connect()` future, its result, the `FailureClass`, the
`ConnectionInfo`, and these steps.

## Provider overview

| Provider | Crate | Data domains | Push | Standards |
| --- | --- | --- | --- | --- |
| **JMAP** | `provider-jmap` | mail read/write/submit, calendar read | EventSource (RFC 8620 §7.3) | RFC 8620, RFC 8621, RFC 8984 (JSCalendar) |
| **IMAP + SMTP** | `provider-imap` | mail read/write (SMTP submit optional) | IMAP `IDLE` (RFC 2177) | RFC 9051, RFC 7162, RFC 2177, RFC 6154, RFC 6851, RFC 4315, RFC 5321/5322, RFC 2047 |
| **CalDAV** | `provider-caldav` | calendar read/write, iMIP inbound RSVP | — | RFC 4791, RFC 5545, RFC 6578, RFC 6764, RFC 6638, RFC 6047 |
| **Microsoft Graph** | `provider-graph` | mail read | — | Microsoft Graph v1.0 mail API |

## Capability matrix

| Capability | JMAP | IMAP | CalDAV | Graph | Notes |
| --- | --- | --- | --- | --- | --- |
| `mail` (read/sync) | yes | yes | — | yes | |
| `mail_writes` | yes | yes | — | — | mark-read/flag, move, delete |
| `message_source` | yes | yes | — | yes | fetch raw RFC 5322 source on demand |
| `submission` | yes | yes* | — | — | *SMTP only when configured |
| `idle` (push) | yes | yes | — | — | EventSource for JMAP, `IDLE` for IMAP |
| `calendars` | yes | — | yes | — | calendar read/sync |
| `calendar_writes` | — | — | yes | — | create/update/delete event resources |

## JMAP

Implements **JMAP Core** (RFC 8620) and **JMAP Mail** (RFC 8621) with full read/write/submit support, plus **JMAP Calendars** read support using **JSCalendar** (RFC 8984) as the normalized projection.

### Supported standards and extensions

- **RFC 8620** — JMAP Core (session resource, method calls, state changes, blob upload/download, EventSource push).
- **RFC 8621** — JMAP Mail (`Mailbox`, `Email`, `EmailSubmission`, `Thread`).
- **RFC 8984** — JSCalendar, the normalized calendar data model.
- **RFC 8620 §7.3** — EventSource push notifications via `JmapWatcher`.

### Capabilities

`mail`, `mail_writes`, `message_source`, `submission`, `idle` (when `eventSourceUrl` is advertised), and `calendars` (read). JMAP calendar **writes** are not implemented; use CalDAV for calendar writes.

### Connection example

```rust
use engine_tls::TlsClientConfig;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};

let tls = TlsClientConfig::bundled();
let config = JmapConfig::new(
    "https://jmap.example.com",
    Credentials::basic("alice@example.com", "app-password"),
)
.with_tls(tls);

let provider = JmapProvider::connect(config).await?;
let info = provider.connection_info();
assert!(info.capabilities.mail());
```

For OAuth providers, use `Credentials::bearer("access-token")`. Servers that genuinely serve their API from a different origin than the session discovery endpoint can use `SessionUrlPolicy::TrustAdvertised` (the default is `RebaseToConnection`, which is correct for reverse-proxied and self-hosted servers).

### Notes

- The JMAP account id is read from the session's `primaryAccounts`, not assumed.
- Raw MIME is **fetched on demand** via the session's `downloadUrl` blob template; it is not eagerly synced.
- Mail edits (`Email/set`) use account-global stable ids, so a move does not change the object's key.

## IMAP + SMTP

Implements a hand-rolled **IMAP4rev2** client (RFC 9051) over a generic async stream, with optional **SMTP** submission (RFC 5321). The crate is intentionally dependency-free for the protocol itself so the transport and parsing can be fully offline-tested.

### Supported standards and extensions

- **RFC 9051** — IMAP4rev2 (the base protocol; the client also works with RFC 3501 servers).
- **RFC 7162** — `CONDSTORE`/`QRESYNC` for incremental flag/expunge deltas.
- **RFC 2177** — IMAP `IDLE` push notifications via `ImapWatcher`.
- **RFC 6154** — `SPECIAL-USE` mailbox roles (`\Inbox`, `\Sent`, `\Drafts`, etc.).
- **RFC 6851** — `MOVE` for atomic server-side moves.
- **RFC 4315** — `UIDPLUS` for per-UID expunge (`UID EXPUNGE`) and `APPENDUID` replies.
- **RFC 2047** — encoded-word decoding in headers and display names.
- **RFC 5321** / **RFC 5322** — SMTP submission and RFC 5322 message assembly.

### Capabilities

`mail`, `mail_writes`, `message_source`, and `idle` (when the server advertises `IDLE`). Submission is advertised only when SMTP is configured.

### Connection example

```rust
use engine_core::ids::MailboxId;
use engine_tls::TlsClientConfig;
use provider_imap::{ImapConfig, ImapProvider};

let tls = TlsClientConfig::bundled();
let mut config = ImapConfig::new(
    "imap.example.com:993",
    "imap.example.com", // SNI / certificate name
    "alice@example.com",
    "app-password",
);

// Optional: sync only mail delivered on or after a date.
config = config.with_since(time::macros::date!(2026-01-01));

// Optional: enable SMTP submission over implicit TLS + AUTH PLAIN.
config = config.with_smtp_tls("smtp.example.com:465", "smtp.example.com");

let provider = ImapProvider::connect(
    &config,
    tls.connector(),
    MailboxId::try_from("INBOX")?,
)
.await?;
```

A plaintext, no-auth SMTP MX (for local fixtures) can be configured with `config.with_smtp("mx.example.com:25")`.

### Notes

- An `ImapProvider` is **bound to one mailbox** for email sync. The folder list syncs at the account level; per-folder email sync is the host's job.
- A mail object's identity is `(mailbox, UIDVALIDITY, UID)`. A copy in another folder is a distinct object.
- On servers that advertise `QRESYNC`, a delta sync reconciles flag changes and expunges in one round trip. Servers without it fall back to new-arrivals-only deltas, with periodic re-snapshots via `Engine::clear_mail_cursors`.
- `IDLE` uses a dedicated connection per watched mailbox; the watcher emits `Changed`/`KeepAlive` events, and the host runs a normal sync on each change.

## CalDAV

Implements **CalDAV** (RFC 4791) calendar read/sync and write, normalizing iCalendar (RFC 5545) resources into the same JSCalendar-shaped engine model that JMAP uses.

### Supported standards and extensions

- **RFC 4791** — CalDAV (calendar access, `PUT`/`DELETE` of event resources).
- **RFC 5545** — iCalendar data format and recurrence model.
- **RFC 6578** — `sync-collection` REPORT for snapshot/delta sync.
- **RFC 6764** — service discovery (`/.well-known/caldav`).
- **RFC 6638** — CalDAV scheduling (server auto-schedule).
- **RFC 6047** — iMIP (iTIP over email), inbound parse + RSVP write primitive.

### Capabilities

`calendars` and `calendar_writes`. Mail methods are not supported.

### Connection example

```rust
use engine_tls::TlsClientConfig;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};

let tls = TlsClientConfig::bundled();
let config = CalDavConfig::new(
    "https://dav.example.com",
    Credentials::Basic {
        username: "alice@example.com".to_owned(),
        password: "app-password".to_owned(),
    },
)
.with_calendar("default")
.with_tls(tls);

let provider = CalDavProvider::connect(config).await?;
```

The `with_calendar` argument is either a name relative to the calendar home (e.g. `"default"`) or an absolute collection path. After listing calendars with `sync_calendars`, you can `rebind` to a different collection without re-running discovery.

### Notes

- A `CalDavProvider` is **bound to one calendar collection** for events. The calendar list syncs at the account level; cross-collection fan-out is the host's job.
- Event identity is the resource href; the iCalendar `UID` is the separate cross-system identifier.
- Writes use conditional `PUT`/`DELETE` (`If-None-Match: *` for creates, `If-Match: "<etag>"` for updates/deletes) for optimistic concurrency.
- The body round-trips the preserved `RawIcal`; the engine does not re-serialize from the lossy projection. For simple creates, the crate provides `provider_caldav::build_event_ical`.
- iMIP inbound parse and the RSVP `PARTSTAT` write primitive are implemented; the full mail-sync wiring and client-side iMIP SMTP delivery are still being integrated.

## Microsoft Graph

Implements **Microsoft Graph v1.0** mail read/sync. Graph is the cloud counterpart to JMAP, but its mail sync is shaped like IMAP: it is per-folder, not account-global.

### Supported standards and extensions

- **Microsoft Graph v1.0** mail API (`/me/mailFolders`, `/me/mailFolders/{id}/messages/delta`).
- `Prefer: IdType="ImmutableId"` so object ids are stable across folder moves.

### Capabilities

`mail` and `message_source`. Submission, push, and calendar are not implemented in this slice.

### Connection example

```rust
use engine_core::ids::MailboxId;
use engine_tls::TlsClientConfig;
use provider_graph::{GraphClient, GraphProvider};

let tls = TlsClientConfig::bundled();
let client = GraphClient::connect("oauth-access-token", &tls)?;
let provider = GraphProvider::new(
    client,
    MailboxId::try_from("inbox-folder-id")?,
);
```

Shared mailboxes can be accessed with `GraphClient::for_mailbox(token, MailboxPrincipal::user("shared@example.com"), &tls)`. Token acquisition and refresh are the host's responsibility.

### Notes

- A `GraphProvider` is **bound to one folder** for email sync. The folder list syncs at the account level; cross-folder fan-out is the host's job.
- Initial sync is a snapshot; subsequent syncs use the per-folder `deltaLink` cursor.
- Changed delta entries that carry only partial properties are re-fetched so the engine always applies whole objects.

## TLS and trust policy

Every provider derives trust from the same `engine_tls::TlsClientConfig`. The default is the bundled Mozilla root program (`TlsClientConfig::bundled()`). Native clients usually want `bundled_and_system()` (bundled roots ∪ OS store), which mirrors Firefox's model.

```rust
use engine_tls::{TlsClientConfig, TlsPolicy};

// Bundled Mozilla roots only (engine default, hermetic).
let tls = TlsClientConfig::bundled();

// Bundled + OS store, for enterprise/MDM CAs.
let tls = engine_tls::client_config(&TlsPolicy::bundled_and_system())?;
```

For IMAP/SMTP, pass `tls.connector()`. For HTTP providers (JMAP/CalDAV/Graph), pass the config itself; the provider uses `tls.reqwest_builder()` internally.

## Capabilities in detail

The `engine_provider::Capabilities` bitset tells the engine what a connected account can do. Hosts should query these flags rather than switching on provider kind.

| Flag | Meaning |
| --- | --- |
| `mail` | The account can read/sync mail metadata. |
| `mail_writes` | The account can mutate mail: mark-read/flag, move, delete. |
| `message_source` | The account can fetch a message's raw RFC 5322 source on demand. |
| `submission` | The account can submit/send new mail. |
| `idle` | The provider can watch for push notifications and emit `WatchEvent`s. |
| `calendars` | The account can read/sync calendars and events. |
| `calendar_writes` | The account can create/update/delete calendar events. |

A read-only JMAP mail account, for example, advertises `mail` but not `mail_writes`. A no-SMTP IMAP account advertises `mail` but not `submission`. An IMAP server without `IDLE` is fully functional on poll.

## Examples and live tests

Each provider crate ships an `examples/` binary that connects to a real server:

- `provider-imap`: `cargo run -p provider-imap --example imap_explore` (read-only; opt-in `IMAP_QRESYNC`, `IMAP_IDLE`, `IMAP_DRAFT`, `IMAP_SEND`).
- `provider-caldav`: `cargo run -p provider-caldav --example caldav_explore` (read-only; opt-in `CALDAV_WRITE`).

JMAP and Graph have gated live tests under `crates/provider-jmap/tests/` and `crates/provider-graph/tests/` that run against the Stalwart Docker harness or a real Graph token, respectively.
