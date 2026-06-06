# Provider Guidance

Provider code is allowed to be messy internally, but must present clean capabilities and changes to the engine.

## Implementation Order

Recommended first provider spine:
1. Reproducible Stalwart Docker protocol harness.
2. JMAP read/write against Stalwart.
3. IMAP/SMTP + CalDAV/CardDAV against the same Stalwart fixture.
4. Optional external-provider smoke tests against real hosted or self-managed servers.

If product pressure changes the order, the domain model tests still need JMAP and JSCalendar coverage before IMAP assumptions land.

## Provider Contract

- Provider adapters return normalized `SyncUpdate` values plus opaque next cursors.
- Adapters own protocol pagination, retries, throttling, and provider quirks.
- The store owns atomic application of changes, cursor persistence, and pending-op reconciliation.
- Capabilities are queried from the adapter. Callers must not switch on provider kind for normal behavior.
- Provider errors should classify retryable, authentication, rate-limit, invalid-state, conflict, and permanent failures.
- Provider adapters must expose whether a sync response is a delta or a complete snapshot.
- Provider object ids may not be stable across container moves; adapters expose a stable or immutable id as the `ProviderKey`, plus a version token (ETag, `changeKey`, MODSEQ) for concurrency.
- Sync cursors are provider-specific (state strings, MODSEQ, sync-tokens, history ids, delta tokens); calendar sync may be inherently time-windowed (a date-bounded view), surfaced as scoped, possibly-incomplete coverage.
- Providers that support push or idle signals emit wake hints; the engine still performs pull sync to fetch changes.

## Stalwart Test Spine

Use Stalwart Docker for deterministic local and CI tests across JMAP, IMAP, SMTP, CalDAV, and CardDAV. The harness must seed one shared dataset that every protocol sees:
- Domain and account credentials.
- Mailboxes/folders and labels where supported.
- Messages with duplicate/missing Message-ID cases, attachments, flags/keywords, and moved/copied messages.
- Calendars with one-off events, recurring events, exceptions, attendees, and virtual locations.
- Contacts/address book entries if CardDAV is in scope.

The JMAP suite must cover:
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
- CalDAV/CardDAV sync uses RFC 6578 sync-token where supported; otherwise CTag plus per-resource ETag diffing.
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge.
- iTIP/iMIP scheduling is distinct from ordinary event storage.

## Fixtures

Fixtures must be deterministic and scrubbed of secrets. Captured live transcripts should record:
- Provider name/version when known.
- Account/server capability responses.
- Exact request/response flow.
- Why the fixture exists and which invariant it protects.
