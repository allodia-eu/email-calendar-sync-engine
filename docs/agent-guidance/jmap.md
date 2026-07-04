# JMAP Client Guidance

This document is authoritative for the **JMAP provider client** — build-order
step 4 (`north-star.md`). It covers the three crates the step added and the JMAP
specifics they implement against the Stalwart fixture. Read it before touching
`engine-provider`, `provider-jmap`, or `engine-sync`, alongside `providers.md`
(the Provider Contract), `store-and-sync.md` (the apply/lease model),
`modeling.md`, `calendar-semantics.md`, and `stalwart-harness.md` (the fixture).

## The three crates

- **`engine-provider`** — the minimal, provider-neutral trait surface. Adapters
  return a normalized [`ScopeSync`] (a `SyncUpdate` + opaque next cursor) or one
  [`SyncPage`] at a time, expose [`Capabilities`], and classify failures with
  [`ProviderError`] over the engine-neutral `FailureClass`. The `Provider` trait is
  **shaped by JMAP** and kept small: only `capabilities` is required. Every
  data-domain method is **default-able** and gated by capability, so an adapter
  implements just the domains it serves — mail providers override `sync_mailboxes`
  + the **paged** `sync_email_page` (plus `mailbox_scope`/`email_scope`); a
  calendar-only provider (`provider-caldav`) overrides `sync_calendars`/
  `sync_events` and leaves the mail methods at their unsupported defaults.
  `sync_email` is a drain over `sync_email_page` (one paged method gets both
  streaming and whole-fetch); `submit_email` defaults to unsupported. `SyncPage` +
  the opaque `PageToken` are the paging vocabulary. Depends only on `engine-core`;
  no network or runtime. Callers never switch on provider kind.
- **`provider-jmap`** — the JMAP/HTTP adapter implementing `Provider`. reqwest +
  rustls (pure-Rust TLS, mobile cross-compile) on tokio. Layers: `transport`
  (auth + HTTP), `request` (the `{using, methodCalls}` envelope, `#id`
  back-references, typed responses), `session` (discovery + URL policy),
  `fetch` (the generic container/member sync **and** the paged `member_page`
  primitive behind `sync_email_page`), `mail`/`calendar`/`json` (normalizers),
  `submit` (sending), `provider` (the trait impl) behind an `Executor` seam.
- **`engine-sync`** — the per-scope loop: `claim → fetch → project/derive →
  apply → release`, with `StaleLease` re-claim-and-recompute and container-
  before-member ordering. `sync_mail`, `sync_calendar` (project + `expand`
  occurrences), and the outbox-mediated `submit_mail`. `sync_mail_streamed` is the
  responsive variant: it commits each email page as it lands (cursor held until the
  last) and notifies a `ProgressSink` (`SyncProgress { scope, fetched, total }`) so
  a host UI can render recent mail and "downloaded Y of X" while a fresh sync fills
  in. The full cross-scope orchestrator (dependency-ordered fan-out, outbox
  workers, tzdata fan-out) is a later step; this is deliberately the minimal driver
  that proves the cycle.

## JMAP specifics implemented

- **Session discovery + URL policy.** The session is fetched (well-known →
  redirect handled), then capabilities, account ids (per `primaryAccounts`, *not*
  assumed), and the core limits are read. Stalwart advertises absolute URLs to its
  configured public host (`https://mail.test.local/`) while a client connects to
  a different origin (the loopback fixture, a reverse proxy); `SessionUrlPolicy`
  resolves this — `RebaseToConnection` (default) keeps the advertised path but
  forces the connection origin, `TrustAdvertised` is RFC-literal for genuinely
  cross-origin providers.
- **Generic container/member fetch.** Containers (`Mailbox`, `Calendar`) sync via
  `Foo/get` (snapshot) or `Foo/changes`→`Foo/get` (delta). Members (`Email`,
  `CalendarEvent`) sync via `Foo/query`→`Foo/get` (snapshot) or `Foo/changes`→
  `Foo/get` (delta). Changed objects are fetched in one round trip via an `#ids`
  result back-reference. The only per-type difference is the method-name prefix,
  the capability set, and the normalizer.
- **Paged member fetch (`member_page` → `sync_email_page`).** Email is fetched one
  page at a time so a streaming host stays responsive. A **snapshot** page is
  `Email/query` sorted `receivedAt` descending (newest first) at a `position` with
  a `limit` and `calculateTotal:true`, then `Email/get` over the page's `#ids`; the
  query ids are the page's `present` set and `next_position` (driven by `total`, or
  a short page when the server omits it) decides whether another page follows. A
  **delta** page is `Email/changes` bounded by `maxChanges`, paging on
  `hasMoreChanges` and resuming from each page's `newState`. `limit` is clamped to
  `maxObjectsInGet` (`0` means "the server's max"). The page's mode + offset/state
  travel in the opaque `PageToken` (`s:<position>` / `d:<state>`), so a recovered or
  continuation page resumes correctly and the engine never parses the token.
- **Delta vs snapshot.** First sync (no cursor) is a snapshot; thereafter a delta,
  recovering to a snapshot on a `cannotCalculateChanges` method error (mapped to
  `FailureClass::NeedsResync`) — recovery happens on the first page, so a recovered
  pass stays a snapshot to its end. Because paging fetches **every** id across all
  pages, a snapshot's accumulated `present` set is complete and tombstones
  correctly; there is no longer a single-page degradation. The orchestrator commits
  intermediate pages additively (cursor held) and applies the tombstoning snapshot
  only on the final page (`store-and-sync.md`).
- **Identity + membership.** JMAP identity is the account-global object id. The
  IMAP COPY surfaces in JMAP as **one** object with two `mailboxIds` (multi-
  membership), while the duplicate-`Message-ID` pair stays **two distinct**
  objects — `Message-ID` is a hint, never identity.
- **Submission.** `Email/set` creates the draft, `EmailSubmission/set` submits it
  (referencing the draft by creation id `#draft`), and `onSuccessUpdateEmail`
  files the sent copy (Drafts→Sent, clear `$draft`). Stalwart **requires an
  `identityId`**, so a send first resolves the Drafts/Sent mailbox ids and the
  identity (`Mailbox/get` + `Identity/get`) before the batched create. The
  `onSuccessUpdateEmail` produces an implicit second `Email/set` response sharing
  the submission's call id. `SetError`s classify through the same `FailureClass`
  taxonomy. Sending is outbox-mediated by `engine-sync::submit_mail`: a durable
  `PendingOp` (carrying the serialized draft, idempotent by `Message-ID`) precedes
  the provider call; the result is recorded under the op lease.
- **Draft attachments.** A draft's attachment bytes are uploaded first (RFC 8620
  §6.1 blob upload — a `POST` of the raw bytes with the part's `Content-Type` to the
  session `uploadUrl` with `{accountId}` substituted), then referenced from the
  `Email/set` `bodyStructure` by the returned `blobId`. Inline (`cid`-referenced)
  parts relate to the body under `multipart/related`; regular files wrap that under
  `multipart/mixed`; the text/HTML values still travel in `bodyValues` while the blob
  parts do not (`crate::submit_body`). A draft with attachments needs a server that
  advertises `uploadUrl` (else a clear session error).
- **Raw source fetch (`fetch_message_source`).** A message's raw RFC 5322 source is
  downloaded on demand through the session `downloadUrl` blob template (RFC 8620
  §6.2): the `{accountId}`/`{blobId}`/`{type}`/`{name}` placeholders are substituted
  (the `blobId` is the one synced onto the object) and the bytes are GET with the
  same credential as every other call. The template's origin is rebased onto the
  connection like `apiUrl`, but **without** URL-parsing it (that would percent-encode
  the `{…}` braces). The `message_source` capability is advertised whenever the
  session exposes mail + a `downloadUrl`. This is what lets a host render a full
  body (and, later, attachments), so JMAP reaches read parity with the IMAP/Graph
  reading path — the source is fetched lazily on first open and cached by the store,
  never synced eagerly.
- **Mail writes (`edit_mail`).** The three provider-neutral edits (`modeling.md`)
  fold onto **one** `Email/set`: `SetKeywords` → a `keywords/<kw>` PatchObject
  (`true` to set, `null` to clear; the `<kw>` is JSON-pointer-escaped, since a JMAP
  keyword may legally contain `/` and `~`), `MoveTo` → a `mailboxIds` **replacement**
  (the message ends up in exactly the destination — the neutral meaning of a move and
  the single-membership common case), `Delete` → a `destroy`. A JMAP id is
  account-global and **stable across a move**, so the receipt key is unchanged and the
  next sync reconciles the new membership (contrast IMAP, which synthesizes a new
  `(mailbox, UIDVALIDITY, UID)` key). A per-object `SetError` (RFC 8620 §5.3)
  classifies through `FailureClass` — `notFound`/`stateMismatch` → `Conflict`
  (re-sync, then retry), matching the IMAP stale-target contract — and a target the
  server silently drops is treated as a `notFound` conflict, never a false success.
  The `mail_writes` capability is advertised whenever the account exposes mail and is
  not `isReadOnly`. Outbox-mediated by `engine-sync::edit_mail` (`crate::mutate`).
- **Push (EventSource → `Watch`).** `JmapWatcher` holds a **dedicated** long-lived
  `text/event-stream` connection to the session `eventSourceUrl` (RFC 8620 §7.3;
  opened `types=Email,Mailbox&closeafter=no&ping=<secs>`), parses the Server-Sent
  Events frames, and maps them onto the provider-neutral `Watch` stream: a `state`
  event whose `StateChange` names a watched JMAP type → `WatchEvent::Changed`, the
  server `ping` keep-alive → `WatchEvent::KeepAlive`. Like IMAP `IDLE` it carries **no
  data** — a `Changed` means only "run the scope's normal sync", the authoritative,
  idempotent reconciliation — so a coalesced/spurious/missed notification cannot
  corrupt the store and a poll-only host stays correct. Reconnection and the
  push-vs-poll policy live in the host. The `idle` capability is advertised whenever
  the session exposes an EventSource endpoint **and** a syncable domain (mail or
  calendars), so a host never opens a watcher whose `Changed` could not map to a
  synced scope (`crate::watch`). Stalwart also
  advertises a WebSocket push channel (`supportsPush`); EventSource is chosen as the
  simpler RFC-8620-core transport over the existing HTTP client.
- **Calendar (read).** `Calendar/get` → `Calendar`; `CalendarEvent/get` →
  JSCalendar `Event`, mapping the time model (`start` + `timeZone` → zoned;
  `timeZone: null` + `showWithoutTime` → all-day date; else floating), recurrence
  (Stalwart emits a **singular** `recurrenceRule`; the plural array is also
  accepted) with overrides, participants, locations, and virtual locations. The
  original JSCalendar payload is preserved as `RawJsCalendar` beside the lossy
  projection. JMAP calendar **writes / RSVP are deferred** (`north-star.md` treats
  JMAP Calendars as the less-deployed transport; CalDAV is step 5).

## Known limitations (documented, not bugs)

- **Raw MIME is fetched on demand, not synced.** Sync ships Tier-1 metadata only;
  the raw RFC 5322 source is downloaded lazily via the `blobId` when a host opens a
  message (`fetch_message_source`, above) and cached by the store thereafter.
  Eager/durable raw-MIME storage *at sync time* is still a later store sub-step.
  Calendar raw (`RawJsCalendar`) *is* preserved (it is a serde field on the object).
- **JMAP calendar writes / RSVP are deferred.** Event `edit_mail`-style writes
  (`CalendarEvent/set` + participant RSVP) are not implemented for JMAP: the
  `north-star.md` treats JMAP Calendars as the less-deployed transport and CalDAV
  (step 5) is the deployed calendar-write path (`provider-caldav`). JMAP calendar
  sync stays **read-only**.
- **Calendar events are still fetched whole**, not paged: only email has a paged
  primitive (`sync_email_page`) so far. Events have no natural recency sort and the
  seed fits one page; when streaming is wanted there, generalize `member_page` with
  a per-type sort and add `sync_events_page`. Snapshot-during-mutation across pages
  remains inherently racy (JMAP gives no cross-query consistency token); the final
  page's cursor is the resume point.
- **JSCalendar verbatim order.** The preserved payload is re-serialized from the
  parsed value, so object key order may normalize; all data survives.

## Testing

- **Offline (always green, no Docker):** secret-free JMAP transcripts captured
  from the harness drive the normalizers; a **fake `Executor`** replays full
  response documents to exercise the snapshot/delta/back-reference/resync
  orchestration, plus **multi-page snapshot and delta chains** (token continuation,
  short-page termination when the server omits `total`), the **`Email/set` mail-write
  flow** (keyword patch / `mailboxIds` move / `destroy`, each with its `SetError`
  classification), and the **blob-upload attachment path** (the fake serves `blobId`s
  and records the upload URL/type/bytes). The **multipart body-structure** assembly is
  unit-tested directly (`submit_body`), and the **EventSource watcher** is driven over
  a **scripted `ChunkSource`** — SSE frame parsing (split chunks, CRLF, comments,
  multi-`data`), `StateChange` classification, watched-type filtering, and the
  `Changed`/`KeepAlive`/closed-stream event loop — all offline. A **blocking mock HTTP
  server** exercises the real transport, session discovery, and `execute`. In
  `engine-sync`, a store-probing fake proves each streamed page is committed and
  host-visible before the next is fetched, a recording `ProgressSink` checks the
  `fetched`/`total` sequence, and a lease-stealer proves a mid-stream `StaleLease`
  restarts safely (the held cursor makes it idempotent). A panic-resistance test
  feeds adversarial JSON through every parser (the `fuzz/` cargo-fuzz counterpart).
- **Live (gated on `STALWART_HTTP_ADDR`, skips otherwise):** `provider-jmap`'s
  `tests/live_provider.rs` (session/mail/calendar/submit, **`edit_mail` flag→move→
  delete on a throwaway message**, **attachment submission** verified through the
  synced-back raw source, and an **EventSource watch** that opens the stream, causes a
  change on a second connection, and asserts a `Changed` arrives) and
  `tests/live_sync.rs` (the full loop through a real `SqliteStore`, asserting the seed
  invariants + search + occurrence expansion, **plus a streamed mail sync** that pages
  the seed three at a time and checks incremental progress). The write tests operate
  on throwaway messages they clean up, so the shared seed the read tests assert on
  stays pristine. Reuses `crates/stalwart-harness`. The `stalwart` CI job runs them;
  both files are excluded from the offline coverage metric, like the harness probes.
- **Fuzzing:** `fuzz/` is a separate cargo-fuzz workspace (`cargo +nightly fuzz
  run jmap_parse`) driving `provider_jmap::fuzz_parse` (behind the `fuzzing`
  feature) over the JSON parse + normalize pipeline.
