# Provider Guidance

Provider code is allowed to be messy internally, but must present clean capabilities and changes to the engine.

## Implementation Order

Recommended first provider spine:
1. Reproducible Stalwart Docker protocol harness. **Implemented** (step 3).
2. JMAP read/write against Stalwart. **Implemented** (step 4); `jmap.md` is
   authoritative for the client (`engine-provider`/`provider-jmap`/`engine-sync`).
   Mail read/sync, submission (**with attachments**), on-demand raw-source fetch,
   **mail writes** (`edit_mail` via `Email/set`), **push** (EventSource `Watch`), and
   **calendar writes** (`CalendarEvent/set` — create/patch/destroy) are all implemented.
   Only participant *RSVP* is deferred for JMAP. Its calendar writes carry **no
   lost-update guard**, and say so (`WriteGuard::Absent`); see `jmap.md`.
3. IMAP/SMTP + CalDAV/CardDAV against the same Stalwart fixture. The **IMAP/SMTP
   mail half is implemented** (step 5a); `imap-smtp.md` is authoritative for the
   `provider-imap` client. **CalDAV calendar read/sync (step 5b) and writes
   (step 5c) are implemented** under `provider-caldav`; `caldav.md` is
   authoritative. **iTIP/iMIP inbound parse/reconcile/trust/apply + the RSVP write
   primitive (step 5d) are implemented** in `engine_core::scheduling` +
   `provider_caldav::imip` (`calendar-semantics.md`/`caldav.md`); the residual
   scheduling deferrals (mail-sync wiring, Scheduling-Inbox `REPORT`, client-iMIP
   SMTP delivery, `ClientImip` persistence) remain. **CardDAV contacts are
   implemented** as a separate `CardDavProvider` sharing only the DAV transport
   with CalDAV.
4. External cloud providers. **Microsoft Graph mail read/sync is implemented**
   under `provider-graph`; `graph.md` is authoritative. Graph's mail sync is
   per-folder (no account-wide message delta), so it follows the IMAP/CalDAV
   container+member shape (a folder-bound provider + `GraphFolderList`/`GraphFolder`
   scopes), not JMAP's account-global one — and unlike JMAP, an incremental `delta`
   returns *partial* changed objects that the adapter re-fetches. It is the first
   adapter validated without the Stalwart fixture: deterministically by a
   fixture-replay HTTP server over scrubbed real captures, plus an optional
   token-gated live test. Graph **submission** (`sendMail` in MIME format) and
   **calendar read/sync + writes** (`calendarView`/delta, `If-Match`-guarded
   create/patch/delete) are also implemented. **Google (Gmail + Google Calendar) is
   implemented** under `provider-google`; `google.md` is authoritative. Unlike Graph,
   Gmail's mail sync is **account-global** (`historyId`, JMAP-like — one
   `GmailMessages` scope, no per-folder fan-out; labels are multi-membership), and
   Google Calendar is **IANA-native** with **RRULE masters** (no Windows-zone table,
   no pre-expanded `calendarView`). Gmail has read/sync + on-demand source + writes +
   send; Google Calendar has read/sync + `If-Match`-guarded writes. Graph
   personal/org/directory contacts and Google Connections/Other
   Contacts/directory/groups are implemented as independent contact sources,
   with optional permissions degrading per source. Same replay + token-gated
   live validation pattern as mail/calendar.
5. Optional further external-provider smoke tests against real hosted or
   self-managed servers.

If product pressure changes the order, the domain model tests still need JMAP and JSCalendar coverage before IMAP assumptions land.

## Provider Contract

- Provider adapters return normalized `SyncUpdate` values plus opaque next cursors.
- Contact adapters implement the separate `ContactsProvider` contract. Discovery
  and source-bound card sync are separate scopes; writes carry neutral
  `ContactDraft`/`ContactPatch` intent, name one destination, and refetch the
  server-canonical card without advancing the normal cursor. Capabilities report
  reads, groups, photos, and contact-write guard strength independently. JMAP and
  Graph are `WriteGuard::Absent`; Google and CardDAV are
  `WriteGuard::Enforced`.
- The streaming primitive is `stream_email(account, cursor, window, fetch_batch, chunk_size) -> EmailStream`: a pull stream of `EmailChunk`s for one sync pass. Each chunk carries a `PassMode` (additive or reconcile, constant across the pass), its `changed` upserts and explicit `removed` keys, a `present` id set (reconcile only — the orchestrator accumulates it to tombstone at end of pass), an optional `total`, and an `advance_to` cursor disposition: additive chunks checkpoint the cursor on **every** commit (so a killed cold backfill resumes where it stopped), reconcile chunks hold it until a final tombstoning chunk. The two knobs are decoupled (`store-and-sync.md`): `fetch_batch` bounds each network round trip, `chunk_size` bounds how many messages a chunk commits/reports — a large batch with a small chunk gives *both* few round trips *and* row-as-it-arrives commits. `sync_email` is a **default drain** over the stream (one combined `SyncUpdate`), so a new adapter implements one streaming method and gets both incremental streaming and whole-scope fetch for free; it drains under `default_sync_window` (the provider's default depth, e.g. an IMAP/Graph `with_since`), while the streaming path takes its `window` **per sync** so a host changes depth without reconnecting.
- `PageToken`/`SyncPage`/`SyncKind` are provider-**internal** paging helpers (an HTTP adapter re-chunks its own whole-page fetch into the stream with `split_page`), not trait surface. Whatever resumes an adapter's fetch — JMAP query position or `Email/changes` state, IMAP UID range, Gmail/Graph page token or delta link — the adapter encodes and decodes it itself; the engine only round-trips the opaque `SyncState` cursor.
- Adapters own protocol pagination, retries, throttling, and provider quirks. Chunks should be ordered so the first ones are the most useful (mail newest-first), since a streaming host renders them as they commit.
- The store owns atomic application of changes, cursor persistence, and pending-op reconciliation. A streaming additive chunk commits as a delta and **checkpoints** the cursor (`ApplyBatch::with_cursor(Some)`); a reconcile pass commits intermediate chunks with the cursor held (`None`) and tombstones on the final chunk (`store-and-sync.md`).
- On-demand content fetch is one provider-neutral method, `fetch_message_source(account, &Message) -> RawMime`, gated by the `message_source` capability (distinct from read `mail` — an adapter can sync envelopes without downloading full bodies, like `submission` vs `mail`). It returns the whole raw RFC 5322 source (headers + every part); the engine extracts displayable text, inline CID parts, and downloadable attachments with `engine-mime`, and caches the raw in the store's content-addressed blob area, so one fetch serves body, inline resources, and attachment downloads (the north-star Tier-3 path). `&Message` carries everything an adapter needs to address it — the `id` key (IMAP `UID FETCH BODY.PEEK[]`) and the `blob_id` (a JMAP/Graph download handle). A stale IMAP target (UID under a changed `UIDVALIDITY`) is a `Conflict` → re-sync then retry. It is driven by `engine_sync::fetch_message_body` / `fetch_inline_parts` / `fetch_message_attachments` / `fetch_message_attachment` (lease-free read-through caches, **not** outbox-mediated — reads need no durable op). (Method + capability **implemented** in `engine-provider`; the IMAP adapter implements it — `imap-smtp.md`; JMAP/Graph overrides via blob download are a later slice.)
- **Post-connect facts are one object.** `Provider::connection_info() -> ConnectionInfo` is the single seam a caller reads after an adapter connects: `{ capabilities, tls_version, http_version }`. There is no separate `capabilities()` method — capabilities are a *field* of the object, so the facts cannot drift apart. Callers must not switch on provider kind for normal behavior. `ConnectionInfo` is `Copy`, so it is returned by value and an adapter may either store it or compose it per call. The two version fields are independently optional because what an adapter can observe is asymmetric (`tls.md`): a `tokio-rustls` adapter (IMAP/SMTP) reports `tls_version` and never `http_version`; a `reqwest` adapter (JMAP/CalDAV/Graph) reports `http_version` and **never** `tls_version` — reqwest exposes only the peer certificate. TLS *policy* stays out of the object: the host chose it and already knows it. `http_version` is the **most recently** observed response's version (`ObservedHttpVersion`), recorded at each transport's single send/collect funnel — *not* the first, because JMAP and CalDAV follow the well-known `30x` themselves and the redirector may be a different origin, and a different negotiated version, from the endpoint that then serves every real request. So JMAP (session discovery) and CalDAV (discovery `PROPFIND`) have a value as soon as `connect` returns and keep it accurate thereafter, while Graph — which issues no request at connect — reports `None` until its first fetch.
- **The connect phase is observable; the connect *state* is not modeled.** `ConnectionInfo` reports the *outcome* of a connect — it is sync and infallible, so it can only describe a connection that already exists. What happened on the way there is reported as it happens, through a `ConnectObserver` an adapter's **config** carries (`ImapConfig`/`JmapConfig`/`CalDavConfig::with_connect_observer`, each holding an `Option<Arc<dyn ConnectObserver>>`). Carrying it on the config, rather than as a `connect` argument, means a host that rebuilds a provider from that config after a dropped session observes the redial for free. The default is no observer, so the seam is additive. The payload is a borrowed `ConnectStep<'_>`: `Redirected { from, to }` (one per well-known `30x` hop the adapter resolves itself), `TlsEstablished(TlsVersion)`, `Authenticated`, `Discovered { endpoint }`. Shape and no-op default (`IgnoreConnectSteps`) mirror `engine-sync`'s `SyncObserver`, including the blanket impl over `Fn`.
  - **Redaction is a type-level guarantee, not a convention.** `Redirected` and `Discovered` carry `Cow<'_, str>` and their variants are `#[non_exhaustive]`, so only `engine-provider` constructs them: an adapter must go through `ConnectStep::redirected`/`::discovered`, which strip the whole `userinfo@` component from a URL's authority. A `Location` or an advertised `apiUrl` may carry `user:pw@`, and these steps exist to feed logs, which `north-star.md` forbids secrets from reaching. A clean URL borrows; a `@` outside the authority (a path, a relative CalDAV href) is untouched.
  - **Steps, never states.** There is deliberately no `Disconnected`/`Connecting`/`Connected` machine in the engine: three of the four adapters are only constructible via a completed `connect()`, so `Connecting` is unobservable through any accessor, and `connection_info()` cannot detect a socket that has since died. A **host** owns that state machine; the engine gives it the inputs — the `connect()` future, its `Ok`/`Err`, the `FailureClass`, the `ConnectionInfo`, and these steps.
  - **What each adapter emits** is asymmetric for the same reasons `ConnectionInfo`'s version fields are (`tls.md`): `provider-imap` → `TlsEstablished` (the only adapter that can — it drives rustls directly) then `Authenticated` after `LOGIN`; `provider-jmap` → `Redirected` per hop, `Authenticated` on the session `2xx`, `Discovered` with the `apiUrl`; `provider-caldav` → `Redirected` per hop, `Discovered` with the calendar-home href (no auth step — credentials ride on each `PROPFIND`, there is no discrete exchange); `provider-graph` → **nothing**, because `GraphClient::connect` performs no I/O. Documented, not faked.
- **Calendar writes are three neutral verbs that carry *intent*, never a protocol payload.** `create_event(account, &EventDraft)`, `patch_event(account, base: &Event, &EventEdit)` and `delete_event(account, &EventDeletion)`, gated by the `calendar_writes` capability. A host states *what the user did* — a title, a new start, which occurrence — and the **adapter** renders it: CalDAV builds/patches an iCalendar document and `PUT`s it, JMAP posts a JSCalendar object or a JSON-pointer PatchObject. So a host never touches a `RawIcal`, an href or an `ETag` to edit an event, and never switches on provider kind. (**Implemented** in `provider-caldav` and `provider-jmap`.)
  - `patch_event`'s `base` is the event **as the caller read it**, and it is load-bearing twice: it carries the provider-native payload the patch applies to (so an update never re-serializes the lossy projection — `calendar-semantics.md`), and the revision the write is guarded by. *Where* the surgery happens is the adapter's business and differs completely: CalDAV has no partial write, so the **client** must rewrite the stored `RawIcal` in place (`patch_event_ical` — fold-aware content-line editing, `DTEND`-vs-`DURATION` exclusion, `SEQUENCE` bookkeeping); JMAP's `update` *is* a patch, so the **server** does it. Only the intent (`EventPatch`/`PatchTarget`) is neutral, which is why the patcher stays in `provider-caldav`.
  - Two rules the neutral intent carries are genuinely universal, and an adapter that gets either wrong corrupts data behind a successful save: **a move must never resolve a zoned event to a UTC instant** (both iCalendar's `DTSTART;TZID=` and JSCalendar's `start`+`timeZone` state the wall clock separately from the zone; writing the instant moves the event for every reader elsewhere and re-times the series at the next DST boundary) — so a form change is *rejected*, not converted (`CalendarDateTime::has_same_form`, in `engine-core` precisely so the two adapters cannot drift). And **series-vs-one-occurrence is a question for the user, never a default** — hence `PatchTarget` has no `Default`.
  - `put_event(account, &EventWrite)` — replace the whole stored document — is **not** part of that spine and is default-unsupported. Only a *document-oriented* transport has such a verb (CalDAV `PUT`; JMAP has none), and only an operation naturally expressed as a finished document should use it: today, the iMIP RSVP primitive. An adapter may advertise `calendar_writes` and still leave it rejecting; the capability covers the neutral spine, not this escape hatch.
- **"Can write" and "can refuse a stale write" are different promises, and only the first is universal.** So `Capabilities::calendar_write_guard() -> Option<WriteGuard>` states which an adapter can keep, and a host reads it **before** writing. `WriteGuard::Enforced` (CalDAV) means a write naming a superseded revision is rejected — `If-Match` → `412`, proven live against both harness servers. `WriteGuard::Absent` (JMAP) means it is **not**: a `CalendarEvent` carries no per-object revision at all, and RFC 8620 §5.3's `ifInState` is scoped to the account's whole type state rather than the object, so it would reject a write because an *unrelated* event changed — the wrong instrument, not merely a broken one (and Stalwart ignores it regardless; `jmap.md`). Under `Absent` a stale edit silently wins, so "the write succeeded" does not mean "no concurrent edit was lost", and a host that cares must detect it above the engine. A neutral write API that *looked* like it gave optimistic concurrency everywhere would be the worst outcome available; this is the type that prevents it.
- **A shared mailbox is just another account, so the seam is *discovery* and nothing else.** A credential can very often open more than its own mailbox — a Microsoft 365 shared mailbox, a JMAP non-personal account, an IMAP folder someone granted you. The engine is already multi-account, so the host onboards such a store under its own `AccountId` with the same credential and every existing sync/store/search path applies unchanged: **no new `SyncScope`, no cursor, no account kind, no SQL migration**. What was genuinely missing was the onboarding step — *which stores can I open?* — which is `Provider::list_shared_mailboxes()` and `resolve_shared_mailbox(address)`, returning `SharedMailbox { handle, address, name, personal }`. Neither takes an `&AccountId`: like `connection_info()`, this is a fact about the **credential**, not about any engine account, and it builds no scope. The `handle` is a `SharedMailboxId` — opaque and adapter-scoped, because the three protocols hand back three different things (a JMAP account id, an IMAP namespace path prefix, a Graph UPN) — and *binding* a provider to it stays per-adapter (`JmapConfig::with_account`, an `ImapProvider` bound to a mailbox under the shared prefix, a Graph `MailboxPrincipal`), because a Graph URL detail must not leak into generic types.
  - **Which verb works is a capability, because the answer differs in kind.** `Capabilities::shared_mailboxes() -> SharedMailboxes` is `Enumerable` (the server lists them: JMAP's session `accounts` map, RFC 8620 §1.6.2; IMAP `NAMESPACE` + `LIST`, RFC 2342), `ByAddress` (it will not list but answers for a named address: Graph has no list route), or `Unsupported` (Gmail — delegation is UI-only; the API route needs a service account with domain-wide delegation, not a user token). A host reads it *before* onboarding, because `Enumerable` affords a pick-from-a-list flow and `ByAddress` requires the user to type an address. Not a boolean, for exactly that reason.
  - **A `SharedMailbox` carries no rights, and no mailbox kind.** Rights, because the account-level signal is unusable: live against Stalwart, an account shared read-only reports `isReadOnly: false` while its only mailbox grants `lr` — so rights live on `Mailbox::access` (`modeling.md`). Kind, because Graph's `userPurpose` is the only such vocabulary any adapter publishes and a delegated credential cannot read it for another mailbox at all (`graph.md`); for the credential's own store `personal` already says it. A field that is always either redundant or unknown is speculative surface.
  - **An address is not identity.** A Microsoft 365 alias resolves to its target mailbox, so two addresses can name one store; the handle is what a host stores and hands back.
- Provider errors should classify retryable, authentication, rate-limit, invalid-state, conflict, and permanent failures.
- Provider adapters must expose whether a sync response is a delta or a complete snapshot.
- Provider object ids may not be stable across container moves; adapters expose a stable or immutable id as the `ProviderKey`, plus a version token (ETag, `changeKey`, MODSEQ) for concurrency.
- Sync cursors are provider-specific (state strings, MODSEQ, sync-tokens, history ids, delta tokens); calendar sync may be inherently time-windowed (a date-bounded view), surfaced as scoped, possibly-incomplete coverage.
- Providers that support push or idle signals emit wake hints; the engine still performs pull sync to fetch changes. This is a **provider-neutral** capability: the `idle` capability flag advertises it, and a `Watch` session (`engine-provider`) yields a `WatchEvent` stream (`Changed` | `KeepAlive`) for one scope. A watch event carries **no data** and is never a source of truth — it means only "run the scope's normal sync," which is the authoritative, idempotent reconciliation. So a missed/coalesced/spurious notification cannot corrupt the store; push only lowers the *latency* of seeing a change, and a poll-only host is fully correct. (**Implemented** for IMAP `IDLE` — `imap-smtp.md` — and for **JMAP EventSource** (`StateChange`, RFC 8620 §7.3) via `JmapWatcher` — `jmap.md`; a Graph webhook is a later slice over the same `Watch` contract.)

## Stalwart Test Spine

Use Stalwart Docker for deterministic local and CI tests across JMAP, IMAP, SMTP, CalDAV, and CardDAV. **Implemented** as build-order step 3 under `docker/stalwart/` (compose + a self-bootstrapping entrypoint that drives Stalwart v0.16's registry setup through its management API, plus a curl seeder) and `crates/stalwart-harness` (readiness + gated smoke suite); `stalwart-harness.md` is authoritative for its design, the bootstrap flow, the per-fixture invariants, the gating contract, and the determinism rules. The harness must seed one shared dataset that every protocol sees:
- Domain and account credentials.
- Mailboxes/folders and labels where supported.
- Messages with duplicate/missing Message-ID cases, attachments, flags/keywords, and moved/copied messages.
- Calendars with one-off events, recurring events, exceptions, attendees, and virtual locations.
- Contacts/address-book entries for shared JMAP/CardDAV parity. The generic and
  adapter behavior is covered offline now; adding these objects to the live
  Stalwart seed remains the harness follow-up tracked in `stalwart-harness.md`.

The JMAP suite must cover (all **implemented** — see `jmap.md`):
- Session discovery and capability detection.
- `Email/changes` with `Email/get` back-references.
- `Mailbox/changes`, `Thread/get`, and state cursor persistence.
- `cannotCalculateChanges` leading to invalidation/full resync.
- Multiple mailbox membership and keyword updates.
- `Email/set` draft creation.
- `EmailSubmission/set` with `onSuccessUpdateEmail`.
- CalendarEvent read/write with JSCalendar recurrence, participants, and virtual locations.
  (Read **and** write: `CalendarEvent/set` create/patch/destroy, including a
  `recurrenceOverrides` edit of one occurrence — all proven live, because the offline fake
  cannot validate a request shape.)
- Provider search fallback for locally incomplete bodies.
- Push/EventSource or equivalent state-change wake hints where available.
  (**Implemented**: `JmapWatcher` over the session `eventSourceUrl` maps
  `StateChange` events onto the neutral `Watch` stream — `jmap.md`.)

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
- Mail mutations (mark-read/flag, move, delete) are one provider-neutral method, `edit_mail(account, &MailEdit) -> MailEditReceipt`, gated by the `mail_writes` capability (distinct from read `mail`, like `calendar_writes` vs `calendars`). `MailEdit` mirrors the three independent mail axes (`modeling.md`): `SetKeywords{add,remove}` (the `$seen`/`$flagged` state), `MoveTo{destination}` (membership — and the mechanism behind a Trash "delete"), and `Delete` (permanent). It is outbox-driven by `engine_sync::edit_mail`, exactly like the calendar writes. JMAP maps all three to one `Email/set` (keywords/mailboxIds patch or `destroy`); IMAP maps them to `UID STORE`, `UID MOVE`, and `UID STORE \Deleted` + `UID EXPUNGE`. A stale target (an IMAP UID under a changed `UIDVALIDITY`) is a `Conflict` → re-sync then retry. (Shape + capability + trait method **implemented** in `engine-provider`; the IMAP adapter implements it — `imap-smtp.md` — as does the JMAP adapter, folding all three edits onto one `Email/set` — `jmap.md`.)
- CalDAV/CardDAV sync uses RFC 6578 sync-token where supported; otherwise CTag plus per-resource ETag diffing. (**Implemented** for the sync-token path in `provider-caldav`; the CTag fallback is a documented follow-up — `caldav.md`.)
- CalDAV writes use ETags and `If-Match`; conflicts refetch before merge. (**Implemented** in `provider-caldav` — the neutral create/patch/delete verbs render as a conditional `PUT` (`If-None-Match`/`If-Match`) + `DELETE`, outbox-driven by `engine_sync::create_calendar_event`/`patch_calendar_event`/`delete_calendar_event`, a `412` → `Conflict`; `caldav.md`. This is the transport that can actually promise the guard — `WriteGuard::Enforced`.)
- iTIP/iMIP scheduling is distinct from ordinary event storage. (**Implemented** for the inbound half: detect (`find_calendar_part`) → parse (`provider_caldav::imip::parse`) → `reconcile`/trust → apply, and the RSVP write primitive (`set_my_partstat` → conditional `PUT` via the existing outbox driver). The CalDAV Scheduling-Inbox `REPORT`, client-iMIP SMTP delivery, and `ClientImip` local-origin persistence stay deferred; `calendar-semantics.md`.)

## Credentials and remote-content URLs

Some URLs an adapter fetches are **not** server-issued endpoints — they come from
remote *content* that a hostile or compromised source controls: a vCard
`PHOTO;VALUE=uri`, a JSContact resource `uri`, a Graph/People photo link. Such a URL
may name any host.

**Never attach the account's `Authorization` header to a URL without first checking
its origin.** Use `engine_provider::same_origin(url, base)`: credentials travel only
to the origin the account is configured against; anything else is fetched
anonymously. This costs nothing in practice — Google serves contact photos publicly
from `googleusercontent.com`, and a card's relative href resolves onto the account's
own origin — while removing the path by which one shared address book could exfiltrate
a user's CardDAV password or OAuth token.

Two traps this rule exists for:
- `Url::join(base, href)` returns an **absolute foreign URL unchanged**, so resolving
  an href against the base does not confine it to the base.
- A same-origin *prefix* is not a same-origin host: `dav.example.com.evil.test` must
  not match `dav.example.com`. `same_origin` compares scheme, host, and effective port
  for equality — never a prefix or substring.

Adapters therefore expose a `get_bytes_unauthenticated` alongside the authenticated
byte fetch, and the client picks between them by origin.

Placeholder substitution into a URL **template** (RFC 8620 §6.2 `downloadUrl`) has the
same shape of hazard: percent-encode every substituted value (RFC 6570 level-1 simple
expansion) so a payload-supplied media type cannot introduce `?`, `#`, `&`, or `/../`
and re-point the request. JMAP ids are already unreserved, so encoding is a no-op for
them; it is the non-id values that matter.

## Fixtures

Fixtures must be deterministic and scrubbed of secrets. Captured live transcripts should record:
- Provider name/version when known.
- Account/server capability responses.
- Exact request/response flow.
- Why the fixture exists and which invariant it protects.
