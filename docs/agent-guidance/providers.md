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
   `provider-imap` client. **CalDAV calendar read/sync (step 5b) and writes
   (step 5c) are implemented** under `provider-caldav`; `caldav.md` is
   authoritative. **iTIP/iMIP inbound parse/reconcile/trust/apply + the RSVP write
   primitive (step 5d) are implemented** in `engine_core::scheduling` +
   `provider_caldav::imip` (`calendar-semantics.md`/`caldav.md`); the residual
   scheduling deferrals (mail-sync wiring, Scheduling-Inbox `REPORT`, client-iMIP
   SMTP delivery, `ClientImip` persistence) and **CardDAV/contacts** land after
   step 5.
4. External cloud providers. **Microsoft Graph mail read/sync is implemented**
   under `provider-graph`; `graph.md` is authoritative. Graph's mail sync is
   per-folder (no account-wide message delta), so it follows the IMAP/CalDAV
   container+member shape (a folder-bound provider + `GraphFolderList`/`GraphFolder`
   scopes), not JMAP's account-global one — and unlike JMAP, an incremental `delta`
   returns *partial* changed objects that the adapter re-fetches. It is the first
   adapter validated without the Stalwart fixture: deterministically by a
   fixture-replay HTTP server over scrubbed real captures, plus an optional
   token-gated live test. Calendar/submission/writes are later slices.
5. Optional further external-provider smoke tests against real hosted or
   self-managed servers.

If product pressure changes the order, the domain model tests still need JMAP and JSCalendar coverage before IMAP assumptions land.

## Provider Contract

- Provider adapters return normalized `SyncUpdate` values plus opaque next cursors.
- The paged primitive is `sync_email_page(account, cursor, page, limit) -> SyncPage<Message>`: one page of changes plus how to continue (`next_page`), the cursor to persist once the *whole* pass applies (`next_cursor`, meaningful only on the last page), and the pass `total` when known. `sync_email` is a **default drain** over it, so a new adapter implements one paged method and gets both incremental streaming and whole-scope fetch for free. A `SyncPage` carries `kind` (snapshot/delta, consistent across the pass), `changed`/`removed`, and — for a snapshot — the `present` ids *this page* covers (the orchestrator accumulates them to tombstone at end of pass).
- `PageToken` is opaque to the engine: the adapter encodes whatever resumes its fetch (JMAP query position or `Email/changes` state, IMAP UID range, Gmail/Graph page token or delta link) and decodes it on the next page. The engine round-trips it without parsing.
- Adapters own protocol pagination, retries, throttling, and provider quirks. Pages should be ordered so the first ones are the most useful (mail newest-first), since a streaming host renders them as they commit.
- The store owns atomic application of changes, cursor persistence, and pending-op reconciliation. Streaming commits each page additively with the cursor held (`ApplyBatch::with_cursor(None)`) and advances it only on the final page (`store-and-sync.md`).
- On-demand body fetch is one provider-neutral method, `fetch_message_source(account, &Message) -> RawMime`, gated by the `message_source` capability (distinct from read `mail` — an adapter can sync envelopes without downloading full bodies, like `submission` vs `mail`). It returns the whole raw RFC 5322 source (headers + every part); the engine extracts displayable text with `engine-mime` and caches the raw in the store's content-addressed blob area, so one fetch serves the plain-text body now and HTML/attachments later (the north-star Tier-3 path). `&Message` carries everything an adapter needs to address it — the `id` key (IMAP `UID FETCH BODY.PEEK[]`) and the `blob_id` (a JMAP/Graph download handle). A stale IMAP target (UID under a changed `UIDVALIDITY`) is a `Conflict` → re-sync then retry. It is driven by `engine_sync::fetch_message_body` (a lease-free read-through cache, **not** outbox-mediated — reads need no durable op). (Method + capability **implemented** in `engine-provider`; the IMAP adapter implements it — `imap-smtp.md`; JMAP/Graph overrides via blob download are a later slice.)
- Capabilities are queried from the adapter. Callers must not switch on provider kind for normal behavior.
- Provider errors should classify retryable, authentication, rate-limit, invalid-state, conflict, and permanent failures.
- Provider adapters must expose whether a sync response is a delta or a complete snapshot.
- Provider object ids may not be stable across container moves; adapters expose a stable or immutable id as the `ProviderKey`, plus a version token (ETag, `changeKey`, MODSEQ) for concurrency.
- Sync cursors are provider-specific (state strings, MODSEQ, sync-tokens, history ids, delta tokens); calendar sync may be inherently time-windowed (a date-bounded view), surfaced as scoped, possibly-incomplete coverage.
- Providers that support push or idle signals emit wake hints; the engine still performs pull sync to fetch changes. This is a **provider-neutral** capability: the `idle` capability flag advertises it, and a `Watch` session (`engine-provider`) yields a `WatchEvent` stream (`Changed` | `KeepAlive`) for one scope. A watch event carries **no data** and is never a source of truth — it means only "run the scope's normal sync," which is the authoritative, idempotent reconciliation. So a missed/coalesced/spurious notification cannot corrupt the store; push only lowers the *latency* of seeing a change, and a poll-only host is fully correct. (**Implemented** for IMAP `IDLE` — `imap-smtp.md`; a JMAP push channel / Graph webhook are later slices over the same `Watch` contract.)

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
- CONDSTORE/QRESYNC paths are optional capabilities, not assumptions. (**Implemented**
  in `provider-imap`: when the server advertises QRESYNC the delta reconciles flag
  changes + expunges via `CHANGEDSINCE`/`VANISHED`; a server without it falls back to a
  new-arrivals delta + periodic snapshot — `imap-smtp.md`.)
- IMAP `IDLE` (RFC 2177) push is an optional capability too, advertised by the `idle`
  flag. (**Implemented** in `provider-imap`: an `ImapWatcher` holds a *dedicated*
  standing connection that turns the `IDLE`/`DONE` keep-alive loop into the neutral
  `Watch` stream — `imap-smtp.md`. A non-`IDLE` server simply isn't watchable, and the
  host polls.)
- IMAP SEARCH is a provider-search fallback when local body coverage is incomplete.
- SMTP post-DATA ambiguity must enter `NeedsConfirmation`; never blind-retry.
- SMTP per-recipient acceptance/rejection before DATA must be represented.
- Sent folder placement must reconcile by generated Message-ID.
- Mail mutations (mark-read/flag, move, delete) are one provider-neutral method, `edit_mail(account, &MailEdit) -> MailEditReceipt`, gated by the `mail_writes` capability (distinct from read `mail`, like `calendar_writes` vs `calendars`). `MailEdit` mirrors the three independent mail axes (`modeling.md`): `SetKeywords{add,remove}` (the `$seen`/`$flagged` state), `MoveTo{destination}` (membership — and the mechanism behind a Trash "delete"), and `Delete` (permanent). It is outbox-driven by `engine_sync::edit_mail`, exactly like the calendar writes. JMAP maps all three to one `Email/set` (keywords/mailboxIds patch or `destroy`); IMAP maps them to `UID STORE`, `UID MOVE`, and `UID STORE \Deleted` + `UID EXPUNGE`. A stale target (an IMAP UID under a changed `UIDVALIDITY`) is a `Conflict` → re-sync then retry. (Shape + capability + trait method **implemented** in `engine-provider`; the IMAP adapter implements it — `imap-smtp.md`.)
- CalDAV/CardDAV sync uses RFC 6578 sync-token where supported; otherwise CTag plus per-resource ETag diffing. (**Implemented** for the sync-token path in `provider-caldav`; the CTag fallback is a documented follow-up — `caldav.md`.)
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge. (**Implemented** in `provider-caldav` — conditional `PUT` (`If-None-Match`/`If-Match`) + `DELETE`, outbox-driven by `engine_sync::write_calendar_event`/`delete_calendar_event`, a `412` → `Conflict`; `caldav.md`.)
- iTIP/iMIP scheduling is distinct from ordinary event storage. (**Implemented** for the inbound half: detect (`find_calendar_part`) → parse (`provider_caldav::imip::parse`) → `reconcile`/trust → apply, and the RSVP write primitive (`set_my_partstat` → conditional `PUT` via the existing outbox driver). The CalDAV Scheduling-Inbox `REPORT`, client-iMIP SMTP delivery, and `ClientImip` local-origin persistence stay deferred; `calendar-semantics.md`.)

## Fixtures

Fixtures must be deterministic and scrubbed of secrets. Captured live transcripts should record:
- Provider name/version when known.
- Account/server capability responses.
- Exact request/response flow.
- Why the fixture exists and which invariant it protects.
