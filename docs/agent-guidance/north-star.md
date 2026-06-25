# PIM Sync Engine North Star

This repository builds a standalone Rust engine for personal information management: mail, calendars, contacts, local search, indexing, and durable writes. It is meant to be embedded by native apps, command-line tools, local daemons, and server-side adapters without changing the core model.

## Product Goal

The engine should provide a local-first, provider-agnostic foundation for:
- Offline mail and calendar access.
- Deterministic sync across modern and legacy protocols.
- Fast structured and full-text search.
- Safe queued writes that survive crashes and network loss.
- Provider-native data preservation for lossless re-derivation.
- Cross-platform bindings for mobile, desktop, CLI, and service hosts.

The Rust core is the product kernel. Apps own UI, account onboarding, OS permissions, notifications, rendering, and platform scheduling. The engine owns sync, normalized domain types, provider adapters, local storage, search/indexing, recurrence, threading, and the outbox.

## Non-Goals

The engine models mail and calendar (and later contacts) objects, sync, search, and writes. It deliberately does not model provider account settings — message rules and filters, vacation/auto-reply, mailbox and working-hours settings — nor free/busy lookup and meeting-time suggestions, cloud-file (Drive/OneDrive) integration, or enterprise data-classification labels. Hosts own these; later phases may add seams if a real need appears.

## Key Decisions

- **JMAP Mail is a first-class model spine.** Its account-global object ids, per-type state changes, batched requests, and submission semantics force a cleaner generic mail model.
- **JSCalendar is the calendar data spine.** JSCalendar is the stable normalized calendar projection. JMAP Calendars is treated as a transport while it remains less widely deployed than CalDAV/iCalendar.
- **Stalwart Docker is the primary protocol test target.** It supports JMAP, IMAP, SMTP, CalDAV, and CardDAV, letting one deterministic fixture validate both modern and legacy protocol paths in local and CI tests.
- **SQLite is the first store.** At-rest encryption is a host-selected seam, not a fixed choice: plain SQLite over OS file encryption (Android FBE / iOS Data-Protection) by default, with SQLCipher as an opt-in whole-database layer. FTS5 and a vector-extension seam cover local search, and the store trait stays encryption-agnostic. Other stores are host adapters.
- **Partial sync is the default.** Headers, metadata, and recent bodies are local first; old bodies and attachments are fetched on demand. Search results must expose coverage metadata so callers can tell a complete answer from a windowed one.
- **FTS works before vectors.** Full-text and structured search must be useful without embeddings. Vector search is local-only by default and remote embedding requires explicit host policy.
- **Provider adapters are isolated crates.** Protocol quirks stay inside provider crates and surface as capabilities, changes, cursors, and transport operations.
- **Writes are outbox mediated.** UI-visible writes first commit locally; background workers perform provider side effects with idempotency and explicit ambiguous states.
- **Raw provider payloads are preserved.** MIME, iCalendar, JSCalendar, and vCard data stay available for re-parsing and protocol-specific writes.

## Workspace Shape

```text
pim-sync-engine/
├── crates/
│   ├── engine-core/             # Domain model, ids, errors, pure logic.
│   ├── engine-sync/             # Sync state machine and orchestration.
│   ├── engine-provider/         # Provider and transport traits.
│   ├── provider-jmap/           # JMAP mail/calendar/contact support.
│   ├── provider-imap/           # IMAP read/sync support.
│   ├── provider-smtp/           # SMTP submission support.
│   ├── provider-caldav/         # CalDAV/CardDAV support.
│   ├── provider-gmail/          # Future Gmail adapter.
│   ├── provider-graph/          # Future Microsoft Graph adapter.
│   ├── engine-store/            # Store trait and contract tests.
│   ├── store-sqlite/            # SQLite, at-rest seam (plain or SQLCipher), FTS5, vectors.
│   ├── engine-search/           # Query AST, ranking, filters, RRF.
│   ├── engine-recurrence/       # Deterministic recurrence -> occurrence expansion (bundled tzdb).
│   ├── engine-index/            # Text extraction, chunks, embedding seam.
│   ├── engine-cli/              # Headless ingestion/search/maintenance harness (CLI host).
│   ├── crypto-keystore/         # Platform credential/key abstraction.
│   ├── engine-api/              # Stable facade consumed by hosts.
│   └── bindings/
│       ├── bindings-uniffi/     # Kotlin/Swift bindings.
│       └── bindings-ffi-c/      # C ABI for desktop/daemon hosts.
└── xtask/                       # Build, fixtures, codegen, CI helpers.
```

`engine-core` must stay I/O-free and async-free. Shared pure contract types such as `SyncUpdate`, `SyncScope`, `SyncState`, `PendingOp`, and `ProviderKey` live in `engine-core` unless a dedicated async-free `engine-contracts` crate becomes clearer. They must not live in `engine-sync`, because both stores and sync orchestration consume them. Provider crates may depend on network/runtime libraries. Host-facing APIs live behind `engine-api`.

## Domain Model Invariants

- Provider object identity and collection membership are separate.
- A stored mail object is a provider object, not a deduplicated RFC822 message.
- IMAP copies in different folders are distinct provider objects, each normally with one membership.
- JMAP/Gmail-style provider objects can have multiple mailbox/label memberships.
- UI/search deduplication is presentation policy, not storage identity.
- Events can have multiple calendar memberships where a provider supports it; one-calendar membership is the common default.
- JMAP keywords and IMAP flags map to message keywords; some provider labels are keywords too, not membership (Gmail `UNREAD`/`STARRED`/`IMPORTANT`).
- Mailboxes, folders, and most labels map to membership, distinct from a collection's normalized role (inbox, sent, drafts, trash, junk, archive).
- Calendar collections map to event membership.
- `Message-ID` is a threading/reconciliation hint, not hard identity.
- IMAP identity includes mailbox, UIDVALIDITY, and UID.
- JMAP identity is account-global and stable.
- One engine instance hosts multiple accounts; `AccountId` scopes every object, sync scope, cursor, and write.
- Search and threading are per-account by default; cross-account unified views are host-composed presentation, not storage-level joins.
- Calendar normalization uses JSCalendar-shaped projections and supports floating times, all-day events, embedded timezones, recurrence rules, recurrence overrides, exclusions, and cross-DST expansion.
- Calendars carry access rights, subscription, owner, default reminders, and color, not only event membership.
- Events carry a kind (default plus provider kinds such as working-location, focus-time, out-of-office); the model preserves kind-specific payload.
- Provider-native raw data is preserved beside normalized projections.
- Provider extended properties and extensions are preserved as normalized, namespaced data, distinct from raw payloads and first-class fields.
- Provider keys are stable across moves; adapters use a provider's immutable-id form where its natural id is not, with a version token (ETag, `changeKey`, MODSEQ) for revisions.
- Attachments span file, item (an embedded message/event), reference (an external/cloud link), and inline (CID) kinds.
- Thread ids carry provenance: provider-assigned where available, locally-derived otherwise. Late-arriving messages can merge local threads.

## Sync Depth And Search Completeness

The default mobile-safe sync policy is tiered:
- **Tier 1:** headers, envelope, flags/keywords, collection membership, provider ids, threading inputs, event metadata.
- **Tier 2:** snippets and recent body text, extracted from plain-text and HTML parts, for FTS.
- **Tier 3:** full bodies and attachments fetched on demand and cached with quotas.

Desktop or server hosts may request fuller replication, but the engine cannot assume it. Search APIs must return coverage metadata. Completeness is several independent axes — local object/content coverage, time-range (recurrence-horizon) coverage, and remote augmentation — not one value; `docs/agent-guidance/search-coverage.md` is authoritative. Provider search fallbacks such as JMAP `Email/query` or IMAP `SEARCH` are capabilities used when local coverage is incomplete.

## Store Contract

The store enforces atomic sync application, outbox reconciliation, fencing-token leases, and snapshot repair. Its guarantees:

- Provider data, derived search/occurrence rows, the next cursor, and pending-op reconciliations for one scope commit in a single transaction, or not at all.
- A `SyncUpdate` is either a delta or a bounded/full snapshot. A snapshot carries the complete current provider id set for its scope so the store can tombstone local rows absent from it.
- At most one effective writer exists per `(account, scope)` and per in-flight outbox op, enforced by store-issued fencing tokens checked inside the write transaction.
- Replaying an update after a crash is idempotent.

`docs/agent-guidance/store-and-sync.md` holds the authoritative `Store` trait signature and the lease, fencing, scope, and reconciliation semantics.

## Search Contract

The engine owns a structured query AST with free text plus filters:
- Mail: `from`, `to`, `cc`, `subject`, `has_attachment`, `before`, `after`, `mailbox`, `label`, `keyword`.
- Calendar: `calendar`, `attendee`, `organizer`, `rsvp`, `location`, `has_conference`, `before`, `after`.

FTS handles text. Normalized tables handle filters. Ranking can combine FTS and vector candidates through reciprocal rank fusion. Search must work without vectors and must report coverage/completeness; see `docs/agent-guidance/search-coverage.md`.

Recurring events are indexed through a bounded `event_occurrence` table generated from normalized calendar data. Range queries use occurrences, not only the master event. The host configures the rolling expansion horizon; edits, timezone data changes, and recurrence-rule changes invalidate affected occurrences.

## Write Contract

Every write is a durable pending operation before any provider side effect:
- Pending operations may depend on earlier operations. Offline create-then-edit flows use local ids and dependency ordering until provider ids are known.
- SMTP sends generate a stable MIME message and Message-ID before submission.
- SMTP recipient handling records pre-DATA partial acceptance/rejection before any DATA phase.
- Ambiguous post-DATA SMTP failures enter `NeedsConfirmation`; the engine never blindly retries a possibly delivered message.
- `NeedsConfirmation` resolves through sync reconciliation, generated Message-ID lookup, or explicit user/host confirmation.
- Sent-folder placement reconciles by generated Message-ID where the provider does not submit and file atomically.
- JMAP submission uses `EmailSubmission/set` and `onSuccessUpdateEmail` when available.
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge.
- Scheduling operations distinguish event storage from iTIP/iMIP message delivery.

## Security And Privacy

Mail and calendar data are hostile input and sensitive data:
- HTML email is sanitized before rendering.
- Remote images are blocked by default and fetched through host policy.
- MIME parsing is fuzz-tested.
- iCalendar and JMAP JSON parsing are fuzz-tested.
- Attachments are quota-managed and opened through host policy.
- Logs, crash reports, snippets, and telemetry are redacted by default.
- Provider credentials never enter the SQL store.
- At-rest protection is host-selected: bulk data relies on OS file encryption by default, with SQLCipher as an opt-in whole-database layer whose key is wrapped by the host platform keystore. High-value secrets (tokens, passwords, key material) are always field-encrypted with a keystore-wrapped key and never stored in cleartext. FTS content and snippets are protected only by whichever at-rest layer is in force.
- Remote embedding is disabled unless an explicit host policy allows content to leave the device/process.
- Export, schema migration, account deletion, credential wipe, and local database compaction are first-class flows.

## Cross-Platform Hosts

- **Android/iOS:** UniFFI Kotlin/Swift bindings; large content crosses FFI through handles, streams, or content-addressed blob APIs, not copied byte arrays.
- **macOS/Windows/Linux:** C ABI or UniFFI where practical; local daemon hosts are acceptable.
- **CLI:** headless harness for fixtures, sync debugging, migrations, search, and outbox replay.
- **Server adapters:** host-owned store, credential, scheduling, and embedding integrations.

The engine exposes a push/wake seam. Providers can emit state-change hints from JMAP push/EventSource, IMAP IDLE, or host notifications; the engine responds by running scoped pull sync. Hosts own OS notification display and background scheduling.

The engine also exposes an injectable clock/time source for recurrence expansion, retry backoff, leases, and confirmation timeouts.

## Build Order

1. **Repository discipline and RFC-backed model.** Add workspace skeleton, strict lint/test policy, model fixtures, and `engine-core` tests first.
2. **SQLite store and search without network.** Implement schema, structured query AST, FTS, recurrence fixtures, and ingestion CLI.
3. **Stalwart Docker harness.** Seed deterministic accounts, messages, calendars, and protocol endpoints for local/CI tests (contacts/CardDAV deferred until contacts land, after step 5). Implemented under `docker/stalwart/` + `crates/stalwart-harness`; see `stalwart-harness.md`.
4. **JMAP read/write.** Implement JMAP sync, JSCalendar normalization, mail submission, calendar writes, RSVP patches, and conference links. **Implemented** for mail (read/sync + submission) and calendar **read** under `engine-provider` + `provider-jmap` + a thin `engine-sync` loop; `jmap.md` is authoritative. JMAP calendar *writes*/RSVP are deferred to a later slice (CalDAV in step 5 is the more-deployed calendar-write path).
5. **IMAP/SMTP + CalDAV/CardDAV.** Implement legacy protocol adapters against the same Stalwart fixture. **IMAP read/sync + SMTP submission are implemented** under `provider-imap` (a mailbox-bound `Provider`; `imap-smtp.md` is authoritative). **CalDAV calendar read/sync is implemented** under `provider-caldav` (a collection-bound `Provider` parsing iCalendar into the same `Event` projection JMAP produces; `caldav.md` is authoritative). The remaining step-5 slices are **CalDAV writes** (PUT/`If-Match`, the more-deployed calendar-write path) and **iTIP/iMIP** scheduling; **CardDAV/contacts** follow after step 5.
6. **Bindings and reference host.** Add the `engine-api` facade, then the UniFFI/CLI/desktop seams over it, in small, tested slices. **The `engine-api` facade is implemented** for store lifecycle and provider-driven mail/calendar sync — an `Engine` over a concrete `SqliteStore` driven by a host `SystemClock`, generic over `Provider` so it stays provider-agnostic; `engine-api.md` is authoritative. Search, the write/outbox surface, streaming progress, and the UniFFI/C-ABI bindings themselves are the remaining slices.
7. **External provider smoke tests.** Add optional live-provider tests only after deterministic protocol tests pass.

Contacts and CardDAV follow the mail and calendar spine rather than leading it: they reuse the provider-object identity and membership model and raw vCard preservation, land after step 5, and are not part of the initial search AST. The repository's mail/calendar focus is deliberate; contact sync and search are additive, not v1 gates.

## Testing Strategy

- Model conformance fixtures for JMAP, JSCalendar, IMAP, SMTP, iCalendar, CalDAV, CardDAV, and MIME.
- Stalwart Docker protocol suite in local and CI (the deterministic fixture; see `stalwart-harness.md`).
- Store contract suite for every store implementation.
- Sync tests for idempotency, resumability, cursor invalidation, membership changes, and recurrence parity.
- Write-path tests for ambiguous sends, duplicate prevention, conflict handling, and outbox transitions.
- Search tests for AST parsing, filters, FTS ranking, RRF, and result stability.
- Property tests for recurrence expansion, query parsing, and rank fusion.
- Fuzzing for MIME, iCalendar, and JMAP JSON parsers.
- Formatting, linting, tests, docs, and coverage gates before merge.
