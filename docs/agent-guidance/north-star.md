# PIM Sync Engine North Star

This repository builds a standalone Rust engine for personal information management: mail, calendars, contacts, local search, indexing, and durable writes. It is embedded by three kinds of host (below) — native apps, command-line tools, local daemons, and server-side services among them — without changing the core model.

## Product Goal

The engine should provide a provider-agnostic foundation for:
- Offline mail, calendar, contact, and unified-people access.
- Deterministic sync across modern and legacy protocols.
- Fast structured and full-text search.
- Safe queued writes that survive crashes and network loss.
- Moving mail, calendars, and contacts between providers as the source had them.
- Per-request access to a backend, for a host that keeps no local copy.
- Provider-native data preservation for lossless re-derivation.
- Cross-platform bindings for mobile, desktop, CLI, and service hosts.

The Rust core is the product kernel. Hosts own UI, account onboarding, OS permissions, notifications, rendering, and platform scheduling. The engine owns sync, normalized domain types, provider adapters, local storage, search/indexing, recurrence, threading, the outbox, and import.

## Hosts

The engine serves three kinds of host. They are shapes of use, not products; the engine names
none.

- **Sync host.** Syncs a window of mail, calendars, and contacts into the engine's local store
  and writes through the outbox, via `engine-api`. A window starts small (months) and widens in
  steps, up to everything.
- **Transfer host.** Moves mailboxes, calendars, and contacts between providers: migration,
  mirroring, later backup. It reads a source through the provider traits and writes the target
  as the source had it through the import seam (`import` feature). It usually moves everything;
  sometimes an administrator sets a window ("the last 2 years"). It keeps its own ledger, not the
  engine store.
- **Direct host.** No sync and no cache. It asks the backend per request (JMAP first), with paged,
  sorted queries the server answers. One process serves thousands of users, and each request
  carries its own credentials. It reaches the engine through `engine-direct`, a planned
  store-less facade.

## Principles

These say where the engine is going. Where today's code differs, "Direction" says when it
changes.

1. **Each protocol is known once.** Request building, the session, keyword mapping, `APPEND`,
   blob transfer: each lives in its adapter, public where a host needs it. No host re-implements
   a protocol.
2. **One question, one answer.** Two code paths that decide the same thing become one.
3. **A capability is a type.** What an adapter can do is a trait it implements, reachable through
   any handle (`Box`, `Arc`, `dyn`), not a flag beside a fat trait whose methods default to
   "unsupported".
4. **Shared vocabulary lives in `engine-core`.** Message metadata, receipts that say what a target
   kept, windows, queries, and outcomes are defined there once. A new field is added once, and
   every path gains it.
5. **Facades compose; they never speak a protocol.** `engine-api` composes the store, sync, the
   outbox, and local search. `engine-direct` composes query, read, write, and upload, with no
   store. A transfer host uses the provider layer directly, and any host may, for instance to
   wrap an adapter.
6. **Every write ends in one of four answers.** Stored, with a receipt of what was kept; may be
   stored, so find it before retrying; never (`item_unavailable`); or try again. Each adapter
   states its point of no return: the step after which a failure means "may be stored".
7. **Credentials are asked for, not held.** One neutral `CredentialSource` is asked on each dial
   or request.
8. **Windows are neutral and widen without a re-read.** A window is one type per domain (mail,
   calendar) for every provider and host. Widening one fetches only what the wider window adds.
9. **The three-host test.** Every engine pull request says how it serves a sync host, a transfer
   host, and a direct host, or why one does not apply.

## Non-Goals

The engine models mail, calendar, and contact objects, sync, search/indexing, writes, and import. Contacts use JSContact as their provider-neutral spine; provider records remain account-scoped and a derived people index supplies cross-account presentation. Filters (message rules) and out-of-office replies are in scope ("Key Decisions"). The engine deliberately does not model other provider account settings — mailbox and working-hours settings — nor free/busy lookup and meeting-time suggestions, cloud-file (Drive/OneDrive) integration, or enterprise data-classification labels. Hosts own these; later phases may add seams if a real need appears.

## Key Decisions

- **JMAP Mail is a first-class model spine.** Its account-global object ids, per-type state changes, batched requests, and submission semantics force a cleaner generic mail model.
- **JSCalendar is the calendar data spine.** JSCalendar is the stable normalized calendar projection. JMAP Calendars is treated as a transport while it remains less widely deployed than CalDAV/iCalendar.
- **Stalwart Docker is the primary protocol test target.** It supports JMAP, IMAP, SMTP, CalDAV, and CardDAV, letting one deterministic fixture validate both modern and legacy protocol paths in local and CI tests.
- **SQLite is the first store.** At-rest encryption is a host-selected seam, not a fixed choice: plain SQLite over OS file encryption (Android FBE / iOS Data-Protection) by default, with SQLCipher as an opt-in whole-database layer. FTS5 and a vector-extension seam cover local search, and the store trait stays encryption-agnostic. Other stores are host adapters.
- **Partial sync is the default.** Headers, metadata, and recent bodies are local first; old bodies and attachments are fetched on demand. Search results must expose coverage metadata so callers can tell a complete answer from a windowed one.
- **FTS works before vectors.** Full-text and structured search must be useful without embeddings. Vector search is local-only by default and remote embedding requires explicit host policy.
- **Provider adapters are isolated crates.** Protocol quirks stay inside provider crates and surface as capabilities, changes, cursors, and transport operations.
- **Writes are outbox mediated.** UI-visible writes first commit locally; background workers perform provider side effects with idempotency and explicit ambiguous states.
- **Import is a seam of its own, behind the `import` feature.** Storing an object that already
  exists, as it was, is what a migration, a webmail's upload into a folder and an app's file
  import share, and it is not a sync verb, so it never changes the sync `Provider` contract
  (`providers.md` → "Import").
- **Raw provider payloads are preserved.** MIME, iCalendar, JSCalendar, JSContact, vCard, and
  provider JSON stay available for re-parsing and protocol-specific writes.
- **Filters and out-of-office replies are in scope.** Message rules and automatic replies get one
  provider-neutral model over Sieve (RFC 5228, with the vacation extension, RFC 5230), JMAP
  `SieveScript` (RFC 9661) and `VacationResponse` (RFC 8621), Graph message rules and automatic
  replies, and Gmail filters and vacation settings. All three host kinds need them: a sync host
  and a direct host show and edit them, and a transfer host moves them between providers. This
  replaces the earlier non-goal that left them to hosts, which would have had each host speak
  four protocols for one setting (principle 1).
- **`engine-direct` is the store-less facade.** A crate in this repository, named for what it is:
  query, read, write, and upload per request, over the provider layer, with no store and no
  cache. It is built once the query seam exists (sort and paging on `MailQuery`, answered by the
  store and by the server alike), because a direct host's mailbox list is that query.
- **The capability-trait split comes after the query seam and the credential source.** Today
  `Provider` is one trait whose data-domain methods default to unsupported and are gated by
  capability flags (`providers.md`, `jmap.md`). Splitting it into a trait per capability, behind
  an adapter handle, breaks every host, so it lands once, as one planned migration with a guide
  per host kind, not piecemeal.
- **New capabilities are reached through accessors on `Provider`.** Each is its own trait, reached through a `Provider` method that defaults to `None`, so a host holding `Box<dyn Provider>` reaches it without knowing the adapter. This is the first step of the capability-trait split and supersedes the rule against sub-traits.
- **Hosts may use the provider layer directly.** A transfer host always does; a sync host may, to
  wrap an adapter. `engine-api` and `engine-direct` are conveniences, not gates.

## Workspace Shape

```text
email-calendar-sync-engine/
├── crates/
│   ├── engine-core/          # Domain model, ids, shared vocabulary. No I/O, no async.
│   ├── engine-provider/      # Provider traits, errors, deadlines.
│   ├── provider-jmap/        # JMAP mail, calendars, contacts.
│   ├── provider-imap/        # IMAP read/sync and SMTP submission.
│   ├── provider-caldav/      # CalDAV and CardDAV.
│   ├── provider-google/      # Gmail, Google Calendar, Google People.
│   ├── provider-graph/       # Microsoft Graph mail, calendar, contacts.
│   ├── engine-http/          # One HTTP send funnel: throttling and deadlines.
│   ├── engine-tls/           # One TLS trust policy for every provider.
│   ├── engine-mime/          # MIME body extraction.
│   ├── engine-rfc5322/       # Outbound message assembly.
│   ├── engine-ical/          # iCalendar parse, serialize, patch.
│   ├── engine-html/          # Mail HTML made safe to render.
│   ├── engine-sieve/         # Sieve scripts split at their commands.
│   ├── engine-recurrence/    # Recurrence expansion (bundled tzdb).
│   ├── engine-search/        # Query AST and DSL, rank fusion, coverage.
│   ├── engine-store/         # Store trait and contract tests.
│   ├── store-sqlite/         # SQLite store, FTS5, content-addressed blobs.
│   ├── engine-sync/          # Sync orchestration.
│   ├── engine-api/           # Sync host facade: store, sync, outbox, local search.
│   ├── engine-direct/        # Planned. Direct host facade: query, read, write, upload.
│   ├── engine-cli/           # CLI host: fixtures, search, debugging.
│   └── dav-cli/, mailbox-fixture/, stalwart-harness/   # Dev and test tools.
├── docker/                   # Stalwart, Dovecot, SabreDAV, Nextcloud fixtures.
├── fuzz/                     # Parser fuzz targets.
└── tools/                    # OAuth and capture tools, outside the workspace.
```

`engine-core` must stay I/O-free and async-free. Shared pure contract types such as `SyncUpdate`, `SyncScope`, `SyncState`, `PendingOp`, and `ProviderKey` live in `engine-core` unless a dedicated async-free `engine-contracts` crate becomes clearer. They must not live in `engine-sync`, because both stores and sync orchestration consume them. Provider crates may depend on network/runtime libraries. A host reaches the engine through a facade — `engine-api` for a sync host, `engine-direct` (planned) for a direct host — or through the provider layer directly, as a transfer host does.

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
- Mail collections carry the caller's access rights too — per collection, because no protocol answers "may I write here?" above it.
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
- **Tier 3:** full bodies and attachments fetched on demand and cached with quotas. **Implemented** for bodies and downloadable attachments, split by *text vs bytes* (`store-and-sync.md`): `Provider::fetch_message_source` returns the raw RFC 5322 source, cached lease-free in a content-addressed filesystem blob area (not SQLite); `engine-mime` extracts the displayable text, which is stored in SQLite and **indexed for search** through a lease-free body FTS, and derives inline CID parts plus selected downloadable attachment bytes from the cached raw source on demand. Surfaced as `Engine::message_body`, `message_inline_parts`, `message_attachments`, and `message_attachment` (and `search_mail` matches the body). Quota/eviction, durable per-attachment blob entities, and embeddings/RAG over the indexed text are later refinements.

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

Every write, whichever host makes it, ends in one of the four answers of principle 6, and each
adapter states its point of no return. A transfer host's import and a direct host's write get the
answer straight from the adapter.

A sync host's write is also a durable pending operation before any provider side effect:
- Pending operations may depend on earlier operations. Offline create-then-edit flows use local ids and dependency ordering until provider ids are known.
- SMTP sends generate a stable MIME message and Message-ID before submission.
- SMTP recipient handling records pre-DATA partial acceptance/rejection before any DATA phase.
- Ambiguous submissions (an SMTP failure after `DATA`, an HTTP submission that failed once it may have reached the server, a timeout at either) enter `NeedsConfirmation`; the engine never blindly retries a possibly delivered message (`deadlines.md`).
- `NeedsConfirmation` resolves through sync reconciliation (its copy in a Sent mailbox), or explicit user/host confirmation (`Store::confirm_pending_op`).
- A send is never lost and never delivered twice whatever ends the process mid-attempt: it records its hand-over before the point of no return, and recovery turns on that record (`store-and-sync.md` → "A send survives its process").
- Sent-folder placement reconciles by generated Message-ID where the provider does not submit and file atomically.
- JMAP submission uses `EmailSubmission/set` and `onSuccessUpdateEmail` when available.
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge.
- Scheduling operations distinguish event storage from iTIP/iMIP message delivery.

## Security And Privacy

Mail and calendar data are hostile input and sensitive data:
- HTML email is sanitized before rendering, by every host with the one sanitiser in `engine-html` (`html.md`); the body fetch itself returns it unsanitized (`mime.md`).
- Remote images are blocked by default and fetched through host policy.
- MIME parsing is fuzz-tested.
- iCalendar and JMAP JSON parsing are fuzz-tested.
- Attachments are quota-managed and opened through host policy.
- Logs, crash reports, snippets, and telemetry are redacted by default. A payload's, a body's or a
  rule value's `Debug` never prints it; an id's or a cursor's own `Debug`, which can carry an
  address or a folder path, prints it only where engine-core is built with debug assertions (the
  host's profile decides). Structs holding content in plain fields, and error texts, can still
  carry it (`modeling.md`, `engine_core::redact`).
- Provider credentials never enter the SQL store.
- TLS trust is unified across every provider and host-selected: one `TlsPolicy` — bundled Mozilla roots by default, with OS/enterprise or explicitly pinned roots as opt-ins — realized by `engine-tls` into one `ring`-backed config (`tls.md`). The engine bakes in no per-provider trust store.
- At-rest protection is host-selected: bulk data relies on OS file encryption by default, with SQLCipher as an opt-in whole-database layer whose key is wrapped by the host platform keystore. High-value secrets (tokens, passwords, key material) are always field-encrypted with a keystore-wrapped key and never stored in cleartext. FTS content and snippets are protected only by whichever at-rest layer is in force.
- Remote embedding is disabled unless an explicit host policy allows content to leave the device/process.
- Export, schema migration, account deletion, credential wipe, and local database compaction are first-class flows.

## Cross-Platform Hosts

- **Android/iOS:** UniFFI Kotlin/Swift bindings; large content crosses FFI through handles, streams, or content-addressed blob APIs, not copied byte arrays.
- **macOS/Windows/Linux:** C ABI or UniFFI where practical; local daemon hosts are acceptable.
- **CLI:** headless harness for fixtures, sync debugging, migrations, search, and outbox replay.
- **Servers:** transfer hosts, direct hosts, and server-side sync hosts. Each owns its scheduling, its embedding integration, and any state of its own, such as a transfer host's ledger, and supplies credentials when the engine asks for them.

The engine exposes a push/wake seam. Providers can emit state-change hints from JMAP push/EventSource, IMAP IDLE, or host notifications; the engine responds by running scoped pull sync. Hosts own OS notification display and background scheduling.

The engine also exposes an injectable clock/time source for recurrence expansion, retry backoff, leases, and confirmation timeouts.

## Build Order

These are the steps the engine was built in so far, with a sync host in mind. "Direction" says what
comes next.

1. **Repository discipline and RFC-backed model.** Add workspace skeleton, strict lint/test policy, model fixtures, and `engine-core` tests first.
2. **SQLite store and search without network.** Implement schema, structured query AST, FTS, recurrence fixtures, and ingestion CLI.
3. **Stalwart Docker harness.** Seed deterministic accounts, messages, calendars, contacts, and
   protocol endpoints for local/CI tests. Implemented under `docker/stalwart/` +
   `crates/stalwart-harness`, including shared JMAP/CardDAV contact fixtures and parity coverage.
   See `stalwart-harness.md`.
4. **JMAP read/write.** Implement JMAP sync, JSCalendar normalization, mail submission, calendar writes, RSVP patches, and conference links. **Implemented** for mail — read/sync, submission (**with attachments**, via RFC 8620 blob upload), on-demand raw-source fetch, **mail writes** (`edit_mail` mark-read/flag/move/delete via `Email/set`), and **push** (EventSource `StateChange` → the neutral `Watch`) — and for calendar, **read and write** (`CalendarEvent/set` create/patch/destroy), under `engine-provider` + `provider-jmap` + a thin `engine-sync` loop; `jmap.md` is authoritative. Being the *second* calendar writer is what finally settled the neutral write API: a host now states an `EventDraft`/`EventPatch` and the adapter renders it, because CalDAV's client-side document surgery and JMAP's server-side patch merge share nothing below the intent. JMAP calendar writes carry **no lost-update guard** and say so (`WriteGuard::Absent`) rather than implying one; participant *RSVP* remains deferred for JMAP.
5. **IMAP/SMTP + CalDAV/CardDAV.** Implement legacy protocol adapters. **IMAP read/sync + SMTP submission are implemented** under `provider-imap`. **CalDAV calendar read/sync and writes and CardDAV address-book/card sync and guarded writes are implemented** under `provider-caldav`; calendar and contact normalizers remain separate while sharing the DAV transport. **iTIP/iMIP inbound parse/reconcile/trust/apply + the RSVP write primitive are implemented** in `engine_core::scheduling` + `provider_caldav::imip`. The residual step-5 scheduling deferrals are mail-sync wiring, the CalDAV Scheduling-Inbox `REPORT`, client-iMIP SMTP delivery, and `ClientImip` local-origin persistence.
6. **Bindings and reference host.** Add the `engine-api` facade, then the UniFFI/CLI/desktop
   seams over it, in small, tested slices. **The Rust facade is implemented** for lifecycle,
   mail/calendar/contact sync, reads/search, streaming progress, durable writes, unified people,
   recipient suggestions, and contact-photo caching. UniFFI/C-ABI bindings remain. `engine-api.md`
   is authoritative.
7. **External provider smoke tests.** Add optional live-provider tests only after deterministic protocol tests pass.

Contacts now form the third provider-neutral domain. They reuse provider-object identity, non-empty membership, atomic scoped sync, the durable outbox, raw document preservation, and capability-driven adapters. `docs/agent-guidance/contacts.md` is authoritative for JSContact normalization, unified people, recipient observations, provider source classes, and the deliberately deferred group/photo mutations.

## Direction

What comes next, in order. Each step passes the three-host test (principle 9).

1. **Now.** One shared message metadata type (keywords and arrival time, categories next), so
   drafts, Sent copies, and import ask for it alike and every receipt reports what was kept in
   it. One IMAP routine that stores a message with its flags and keywords, used by every path
   that stores one. The mail import seam (`import` feature) and IMAP import.
2. **Next.** JMAP import (`Email/import`). CalDAV and CardDAV import. The JMAP client's
   primitives (session, request building, blob transfer) made public, so no host rebuilds them.
   One `CredentialSource`. The window vocabulary, with widening that does not re-read what is
   already held. The filters and out-of-office model: the reply over JMAP has landed
   (`Provider::vacation`); rules over JMAP Sieve are next.
3. **Then.** Sort and paging on `MailQuery`, answered by the local store and by the server alike.
   `engine-direct` over it. The capability-trait split, as one migration with a guide per host
   kind.

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
