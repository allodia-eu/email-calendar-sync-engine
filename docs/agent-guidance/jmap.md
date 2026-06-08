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
  **shaped by JMAP** and kept small: required `sync_mailboxes` + the **paged**
  `sync_email_page` + `mailbox_scope`/`email_scope`; default-able `sync_email` (a
  drain over `sync_email_page`, so an adapter implements one paged method and gets
  both streaming and whole-fetch), `submit_email`, `sync_calendars`/`sync_events`,
  and the calendar scope accessors (a non-JMAP provider overrides). `SyncPage` +
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
- **Calendar (read).** `Calendar/get` → `Calendar`; `CalendarEvent/get` →
  JSCalendar `Event`, mapping the time model (`start` + `timeZone` → zoned;
  `timeZone: null` + `showWithoutTime` → all-day date; else floating), recurrence
  (Stalwart emits a **singular** `recurrenceRule`; the plural array is also
  accepted) with overrides, participants, locations, and virtual locations. The
  original JSCalendar payload is preserved as `RawJsCalendar` beside the lossy
  projection. JMAP calendar **writes / RSVP are deferred** (`north-star.md` treats
  JMAP Calendars as the less-deployed transport; CalDAV is step 5).

## Known limitations (documented, not bugs)

- **Raw MIME is referenced, not stored.** A mail object keeps its `blobId` for
  on-demand fetch, but durable raw-MIME blob storage awaits the store's blob
  sub-step, so step 4 syncs Tier-1 metadata only. Calendar raw (`RawJsCalendar`)
  *is* preserved (it is a serde field on the object).
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
  short-page termination when the server omits `total`); a **blocking mock HTTP
  server** exercises the real transport, session discovery, and `execute`. In
  `engine-sync`, a store-probing fake proves each streamed page is committed and
  host-visible before the next is fetched, a recording `ProgressSink` checks the
  `fetched`/`total` sequence, and a lease-stealer proves a mid-stream `StaleLease`
  restarts safely (the held cursor makes it idempotent). A panic-resistance test
  feeds adversarial JSON through every parser (the `fuzz/` cargo-fuzz counterpart).
- **Live (gated on `STALWART_HTTP_ADDR`, skips otherwise):** `provider-jmap`'s
  `tests/live_provider.rs` (session/mail/calendar/submit) and `tests/live_sync.rs`
  (the full loop through a real `SqliteStore`, asserting the seed invariants +
  search + occurrence expansion, **plus a streamed mail sync** that pages the seed
  three at a time and checks incremental progress). Reuses `crates/stalwart-harness`.
  The `stalwart` CI job runs them; both files are excluded from the offline coverage
  metric, like the harness probes.
- **Fuzzing:** `fuzz/` is a separate cargo-fuzz workspace (`cargo +nightly fuzz
  run jmap_parse`) driving `provider_jmap::fuzz_parse` (behind the `fuzzing`
  feature) over the JSON parse + normalize pipeline.
