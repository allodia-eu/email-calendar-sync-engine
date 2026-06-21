# Provider Guidance

Provider code is allowed to be messy internally, but must present clean capabilities and changes to the engine.

## Implementation Order

Recommended first provider spine:
1. Reproducible Stalwart Docker protocol harness. **Implemented** (step 3).
2. JMAP read/write against Stalwart. **Implemented** (step 4); `jmap.md` is
   authoritative for the client (`engine-provider`/`provider-jmap`/`engine-sync`).
   JMAP calendar *writes*/RSVP are deferred to a later slice.
3. IMAP/SMTP + CalDAV/CardDAV against the same Stalwart fixture. The **IMAP/SMTP
   mail half is implemented** (step 5a); `imap-smtp.md` is authoritative for the
   `provider-imap` client. **CalDAV calendar read/sync is implemented** (step 5b)
   under `provider-caldav`; `caldav.md` is authoritative. The remaining step-5
   slices are **CalDAV writes** + **iTIP/iMIP**; **CardDAV/contacts** land after
   step 5.
4. Optional external-provider smoke tests against real hosted or self-managed servers.

If product pressure changes the order, the domain model tests still need JMAP and JSCalendar coverage before IMAP assumptions land.

## Provider Contract

- Provider adapters return normalized `SyncUpdate` values plus opaque next cursors.
- The paged primitive is `sync_email_page(account, cursor, page, limit) -> SyncPage<Message>`: one page of changes plus how to continue (`next_page`), the cursor to persist once the *whole* pass applies (`next_cursor`, meaningful only on the last page), and the pass `total` when known. `sync_email` is a **default drain** over it, so a new adapter implements one paged method and gets both incremental streaming and whole-scope fetch for free. A `SyncPage` carries `kind` (snapshot/delta, consistent across the pass), `changed`/`removed`, and — for a snapshot — the `present` ids *this page* covers (the orchestrator accumulates them to tombstone at end of pass).
- `PageToken` is opaque to the engine: the adapter encodes whatever resumes its fetch (JMAP query position or `Email/changes` state, IMAP UID range, Gmail/Graph page token or delta link) and decodes it on the next page. The engine round-trips it without parsing.
- Adapters own protocol pagination, retries, throttling, and provider quirks. Pages should be ordered so the first ones are the most useful (mail newest-first), since a streaming host renders them as they commit.
- The store owns atomic application of changes, cursor persistence, and pending-op reconciliation. Streaming commits each page additively with the cursor held (`ApplyBatch::with_cursor(None)`) and advances it only on the final page (`store-and-sync.md`).
- Capabilities are queried from the adapter. Callers must not switch on provider kind for normal behavior.
- Provider errors should classify retryable, authentication, rate-limit, invalid-state, conflict, and permanent failures.
- Provider adapters must expose whether a sync response is a delta or a complete snapshot.
- Provider object ids may not be stable across container moves; adapters expose a stable or immutable id as the `ProviderKey`, plus a version token (ETag, `changeKey`, MODSEQ) for concurrency.
- Sync cursors are provider-specific (state strings, MODSEQ, sync-tokens, history ids, delta tokens); calendar sync may be inherently time-windowed (a date-bounded view), surfaced as scoped, possibly-incomplete coverage.
- Providers that support push or idle signals emit wake hints; the engine still performs pull sync to fetch changes.

## Stalwart Test Spine

Use Stalwart Docker for deterministic local and CI tests across JMAP, IMAP, SMTP, CalDAV, and CardDAV. **Implemented** as build-order step 3 under `docker/stalwart/` (compose + a self-bootstrapping entrypoint that drives Stalwart v0.16's registry setup through its management API, plus a curl seeder) and `crates/stalwart-harness` (readiness + gated smoke suite); `stalwart-harness.md` is authoritative for its design, the bootstrap flow, the per-fixture invariants, the gating contract, and the determinism rules. The harness must seed one shared dataset that every protocol sees:
- Domain and account credentials.
- Mailboxes/folders and labels where supported.
- Messages with duplicate/missing Message-ID cases, attachments, flags/keywords, and moved/copied messages.
- Calendars with one-off events, recurring events, exceptions, attendees, and virtual locations.
- Contacts/address book entries if CardDAV is in scope. (Deferred: contacts land after step 5, so the step-3 seed covers mail + calendar only; CardDAV is added without rework when contacts arrive.)

The JMAP suite must cover (all **implemented** in step 4 — see `jmap.md`; the
calendar suite is read-only for now):
- Session discovery and capability detection.
- `Email/changes` with `Email/get` back-references.
- `Mailbox/changes`, `Thread/get`, and state cursor persistence.
- `cannotCalculateChanges` leading to invalidation/full resync.
- Multiple mailbox membership and keyword updates.
- `Email/set` draft creation.
- `EmailSubmission/set` with `onSuccessUpdateEmail`.
- CalendarEvent read/write with JSCalendar recurrence, participants, and virtual locations.
- Provider search fallback for locally incomplete bodies.
- Push/EventSource or equivalent state-change wake hints where available.

## IMAP/SMTP/CalDAV Requirements

Run the first deterministic IMAP/SMTP/CalDAV tests against Stalwart. Add external-provider smoke tests later for provider drift, not as the first correctness gate.

- IMAP identity includes mailbox, UIDVALIDITY, and UID.
- UIDVALIDITY reset invalidates the scope and triggers rediscovery.
- CONDSTORE/QRESYNC paths are optional capabilities, not assumptions.
- IMAP SEARCH is a provider-search fallback when local body coverage is incomplete.
- SMTP post-DATA ambiguity must enter `NeedsConfirmation`; never blind-retry.
- SMTP per-recipient acceptance/rejection before DATA must be represented.
- Sent folder placement must reconcile by generated Message-ID.
- CalDAV/CardDAV sync uses RFC 6578 sync-token where supported; otherwise CTag plus per-resource ETag diffing. (**Implemented** for the sync-token path in `provider-caldav`; the CTag fallback is a documented follow-up — `caldav.md`.)
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge. (Deferred: the read slice preserves each event's `ETag` so the write slice can `If-Match` without a refetch.)
- iTIP/iMIP scheduling is distinct from ordinary event storage. (Deferred to a later slice; the model lives in `engine_core::scheduling`.)

## Fixtures

Fixtures must be deterministic and scrubbed of secrets. Captured live transcripts should record:
- Provider name/version when known.
- Account/server capability responses.
- Exact request/response flow.
- Why the fixture exists and which invariant it protects.
