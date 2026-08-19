# JMAP Client Guidance

This document is authoritative for the **JMAP provider client** — build-order
step 4 (`north-star.md`). It covers the three crates the step added and the JMAP
specifics they implement against the Stalwart fixture. Read it before touching
`engine-provider`, `provider-jmap`, or `engine-sync`, alongside `providers.md`
(the Provider Contract), `store-and-sync.md` (the apply/lease model),
`modeling.md`, `calendar-semantics.md`, and `stalwart-harness.md` (the fixture).

## The three crates

- **`engine-provider`** — the minimal, provider-neutral trait surface. Adapters
  return a normalized [`ScopeSync`] (a `SyncUpdate` + opaque next cursor) or stream
  one email pass as [`EmailChunk`]s, expose their post-connect facts as one
  [`ConnectionInfo`] (capabilities + negotiated transport versions — `providers.md`),
  and classify failures
  with [`ProviderError`] over the engine-neutral `FailureClass`. The `Provider` trait
  is **shaped by JMAP** and kept small: only `connection_info` is required. Every
  data-domain method is **default-able** and gated by capability, so an adapter
  implements just the domains it serves — mail providers override `sync_mailboxes`
  + the **streaming** `stream_email` (plus `default_sync_window`,
  `mailbox_scope`/`email_scope`); a calendar-only provider (`provider-caldav`)
  overrides `sync_calendars`/`sync_events` and leaves the mail methods at their
  unsupported defaults. `sync_email` is a drain over `stream_email` (one streaming
  method gets both incremental streaming and whole-fetch); `submit_email` defaults to
  unsupported. `SyncPage`/`PageToken`/`SyncKind` survive as provider-**internal**
  paging helpers (each adapter re-chunks its own page fetch with `split_page`), not
  trait surface. Depends only on `engine-core`; no network or runtime. Callers never
  switch on provider kind.
- **`provider-jmap`** — the JMAP/HTTP adapter implementing `Provider`. reqwest +
  rustls (pure-Rust TLS, mobile cross-compile) on tokio, built from the shared
  per-account `TlsClientConfig` in `JmapConfig` (`tls.md`). Layers: `transport`
  (auth + HTTP), `request` (the `{using, methodCalls}` envelope, `#id`
  back-references, typed responses), `session` (discovery + URL policy),
  `fetch` (the generic container/member sync **and** the paged `member_page`
  primitive `stream_email` re-chunks), `mail`/`calendar`/`json` (normalizers),
  `submit` (sending), `provider` (the trait impl) behind an `Executor` seam.
- **`engine-sync`** — the per-scope loop: `claim → fetch → project/derive →
  apply → release`, with `StaleLease` re-claim-and-recompute and container-
  before-member ordering. `sync_mail`, `sync_calendar` (project + `expand`
  occurrences), and the outbox-mediated `submit_mail`. `sync_mail` is the responsive
  path: it commits each email chunk as
  it lands — an **additive** pass (cold backfill or delta) checkpoints the cursor
  per chunk (resumable), a **reconcile** re-snapshot holds it until the tombstoning
  final chunk — and notify a `SyncObserver` with a `SyncCommit { scope, fetched,
  total, upserted, removed }` (progress *and* the exact rows that changed) so a host
  renders recent mail, a live "downloaded Y of X", and splices its view **without
  re-querying**. `StreamTuning` decouples the fetch batch (round trips) from the
  commit chunk (granularity) and carries the per-sync depth `window`;
  `AccountProgress` folds per-folder commits into one account-level figure. The full
  cross-scope orchestrator (dependency-ordered fan-out, outbox workers, tzdata
  fan-out) is a later step; this is deliberately the minimal driver that proves the
  cycle.

## JMAP specifics implemented

- **Session discovery + URL policy.** The session is fetched (well-known →
  redirect handled), then capabilities, account ids (per `primaryAccounts`, *not*
  assumed), and the core limits are read. `JmapClient::connect` reports the phase to
  the config's `ConnectObserver` (`providers.md`): one `ConnectStep::Redirected` per
  hop it resolves itself (both sides already rebased, so a host sees the hop it could
  replay), `ConnectStep::Authenticated` when the session responds `2xx` with the
  account's credentials attached, and `ConnectStep::Discovered` naming the resolved
  `apiUrl` that will serve every method call. No `TlsEstablished` — reqwest never
  exposes the negotiated version (`tls.md`). Under `RebaseToConnection` every one of
  those URLs derives from the connection base, so userinfo on the base would propagate
  into each step; `ConnectStep`'s constructors scrub it. Stalwart advertises absolute URLs to its
  configured public host (`https://mail.test.local/`) while a client connects to
  a different origin (the loopback fixture, a reverse proxy); `SessionUrlPolicy`
  resolves this — `RebaseToConnection` (default) keeps the advertised path but
  forces the connection origin, `TrustAdvertised` is RFC-literal for genuinely
  cross-origin providers. **The rebase is scoped to the session's own advertised
  origin.** A session may legitimately span two: Fastmail serves `apiUrl` from
  `api.fastmail.com` and `downloadUrl` from `www.fastmailusercontent.com`, a separate
  cookie-less origin for untrusted user content. A public-hostname mismatch applies
  uniformly to one server's own origin and cannot explain a second, so a URL whose
  origin differs from the advertised `apiUrl`'s is kept **verbatim** — rewriting it can
  only aim the request at a path the connection host does not route. It did exactly
  that: every Fastmail message-source download hit a catch-all `302` to a marketing
  page, so no JMAP body ever cached.
- **Generic container/member fetch.** Containers (`Mailbox`, `Calendar`) sync via
  `Foo/get` (snapshot) or `Foo/changes`→`Foo/get` (delta). Members (`Email`,
  `CalendarEvent`) sync via `Foo/query`→`Foo/get` (snapshot) or `Foo/changes`→
  `Foo/get` (delta). Changed objects are fetched in one round trip via an `#ids`
  result back-reference. The only per-type difference is the method-name prefix,
  the capability set, and the normalizer.
- **Streaming member fetch (`stream_email` re-chunks `member_page`).** Email streams
  as `EmailChunk`s so a host stays responsive. The JMAP round trip is atomic (a page
  arrives whole), so `stream_email` loops `member_page` and re-chunks each page with
  `split_page` — a **snapshot** page becomes `Reconcile` chunks, a **delta** page
  `Additive` chunks — then yields a final marker chunk carrying the cursor. JMAP is
  not cheaply resumable mid-pass, so intermediate chunks **hold** the cursor
  (`additive_held`) and a crash re-runs the pass. A **snapshot** page is `Email/query`
  sorted `receivedAt` descending (newest first) at a `position` with a `limit` and
  `calculateTotal:true`, then `Email/get` over the page's `#ids`; the query ids are the
  page's `present` set and `next_position` (driven by `total`, or a short page when the
  server omits it) decides whether another page follows. A **delta** page is
  `Email/changes` bounded by `maxChanges`, paging on `hasMoreChanges` and resuming from
  each page's `newState`. The streaming knobs map on: `fetch_batch` is the per-page
  `limit` (clamped to `maxObjectsInGet`; `0` = the server's max), `chunk_size` is how
  many messages `split_page` emits per commit. The page's mode + offset/state travel in
  the opaque `PageToken` (`s:<position>` / `d:<state>`) — a provider-**internal** helper
  now, not trait surface — so a continuation page resumes and the engine never parses it.
- **Sync-depth window (JMAP now windows).** `stream_email` takes a `SyncWindow { since }`
  **per sync** and threads it into the snapshot `Email/query` as an `after` filter on
  `receivedAt` (RFC 8621 §4.4.1), so a large mailbox syncs only recent mail — JMAP is no
  longer the "can't window" provider (the depth is no longer baked in at construction).
  `Email/changes` takes no filter, so a delta carries an old message moved into scope; the
  engine drops it on apply (`SyncWindow::admits`, `store-and-sync.md`), which is where the
  bound holds for every adapter. `default_sync_window` (the full history) backs the
  whole-scope `sync_email` drain.
- **Delta vs snapshot.** First sync (no cursor) is a snapshot; thereafter a delta,
  recovering to a snapshot on a `cannotCalculateChanges` method error (mapped to
  `FailureClass::NeedsResync`) — recovery happens on the first page, so a recovered
  pass stays a snapshot to its end. Because paging fetches **every** id across all
  pages, a snapshot's accumulated `present` set is complete and tombstones
  correctly; there is no longer a single-page degradation. The orchestrator commits
  intermediate chunks additively (cursor held) and applies the tombstoning snapshot
  only on the final chunk (`store-and-sync.md`).
- **An `Email/changes` `updated` id is a state change, never a content change.** RFC 8621
  §4.1 makes `keywords` and `mailboxIds` the *only* mutable `Email` properties — the bytes are
  immutable, and editing a draft creates a new object — so an id in `updated` cannot be
  reporting a content change. The adapter resolves those ids with an `Email/get` for
  `EMAIL_STATE_PROPERTIES` (`id`, `keywords`, `mailboxIds`) and emits a `MailStateChange`;
  `created` ids are fetched whole. The store then writes the message row and the `membership`
  junction and leaves the payload alone, so a mark-read cannot destroy a field the change had
  no way to send.

  Filing is carried (`MailState::mailboxes` is `Some`) because JMAP moves a message **in
  place**, under a stable id — a keyword-only change would silently lose an archive. It is
  also how a message becomes *sent*: `EmailSubmission/set` moves the draft with
  `onSuccessUpdateEmail`, so the send arrives as an `updated` id for a key the store already
  holds. Both are pinned live (`live_state_delta.rs`, `live_sent_observation.rs`).
- **Identity + membership.** JMAP identity is the account-global object id. The
  IMAP COPY surfaces in JMAP as **one** object with two `mailboxIds` (multi-
  membership), while the duplicate-`Message-ID` pair stays **two distinct**
  objects — `Message-ID` is a hint, never identity.
- **Date-typed fields.** RFC 8620 §1.4 distinguishes `UTCDate` (Z-only) from
  `Date` (a full RFC 3339 `date-time` that may carry a numeric offset). Honor the
  distinction per property: `receivedAt` (and JSCalendar `created`/`updated`) are
  `UTCDate` and parse strictly through `json::datetime`; **`sentAt`** is a `Date`
  (RFC 8621 §4.1.1 — the message `Date` header, which servers such as Fastmail
  emit in the sender's local offset), so `json::sent_at` parses it through
  `UtcDateTime::parse_rfc3339` (accepts `Z` **or** `±hh:mm`, normalizing to UTC).
  Blast radius: a malformed **optional** per-message field degrades that one field
  (`sentAt` → `None`) and never aborts the mailbox sync — do not re-tighten it to a
  `?` that raises a `Permanent` error for the whole page (issue #38).
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
  connection like `apiUrl` — but only when it *is* the `apiUrl`'s origin (see the
  session bullet above; Fastmail's is not), and **without** URL-parsing it (that would
  percent-encode the `{…}` braces). The `message_source` capability is advertised whenever the
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
  projection.
- **There is no JSCalendar version to negotiate. Read the shape, do not ask.** This is the
  durable answer to "which version does this server speak?", and the answer is that the
  question cannot be asked:
  - **The session says nothing.** `urn:ietf:params:jmap:calendars` is an *empty object* in
    the JMAP Session `capabilities`, and its account-level properties
    (`maxCalendarsPerEvent`, `minDateTime`, `maxDateTime`, `maxExpandedQueryDuration`,
    `maxParticipantsPerEvent`, `mayCreateCalendar`) are **byte-identical between
    draft-24 and draft-27** — verified by diffing the two drafts in full. Nothing in the
    session distinguishes them, by design rather than oversight.
  - **The only in-band signal is the object's own `version`** (JSCalendar 2.0 /
    jscalendarbis §3.1.2, value `"2.0"`). **Do not send it.** Stalwart v0.16.15 neither
    emits it nor accepts it: a `CalendarEvent/set` create carrying `version` is rejected
    with `invalidProperties: ["version"]`, and asking for it in `properties` returns
    nothing. So "support the latest draft" cannot mean "emit the 2.0 markers".
  - **What differs in practice is one spelling**, and it is fallback-shaped rather than
    switch-shaped. A participant's address is `calendarAddress` in 2.0 and `sendTo`
    (a map of *method* → URI; `imip` is the mail one) in 1.0 — and 2.0 **reserves**
    `sendTo`, so no server sets both. `calendar::participant_address` reads 2.0 then falls
    back to 1.0, which needs no version detection and survives the draft moving again. The
    same applies to the event's organizer (`organizerCalendarAddress` in 2.0, `replyTo` in
    1.0), which the projection does not read today — the organizer is a participant with the
    owner role.
  - **Writes stay on the intersection.** Everything `calendar_write` sends (`@type`, `uid`,
    `title`, `start`/`timeZone`/`showWithoutTime`, `duration`, `locations`) is spelled the
    same in both versions, so there is nothing to choose. That stops being true the moment a
    write carries participants, and *that* is when a version decision has to be made — from
    the shape read back, not from a capability.
  - **Stalwart is the only JMAP calendar server we can test any of this against.** It serves
    2.0 shapes (`calendarAddress`, `organizerCalendarAddress`) and its docs cite
    draft-ietf-jmap-calendars + jscalendarbis without pinning a number. Fastmail — the draft's
    own authors — serve **no** JMAP calendars to third parties at all: their developer page
    says calendars are CalDAV-only and JMAP access is coming "as soon as the specification is
    finalized", and their API tokens have no Calendars scope to grant (a request for the
    capability is refused `403 unknownCapability`). So the 1.0 fallback above is
    **spec-derived (RFC 8984 §4.4.6), not observed** — there is currently no server to
    observe it on, and that is a disclosure, not an omission.
- **Calendar writes (`CalendarEvent/set`).** The three neutral write verbs
  (`providers.md`) fold onto one `CalendarEvent/set` (`crate::calendar_write`):
  `create_event` → a `create` of a JSCalendar object under a fixed creation id, whose
  **server-assigned** id comes back in `created` and is the only place the caller can learn
  it; `patch_event` → an `update` PatchObject; `delete_event` → a `destroy`. The
  `calendar_writes` capability is advertised whenever the account exposes calendars and is
  not `isReadOnly`, exactly as `mail_writes` is.
  - **The server does the surgery, so this adapter has no serializer.** JMAP's `update` *is*
    a patch — a JSON-pointer PatchObject (RFC 8620 §5.3) the server merges into the stored
    object. Verified live, not assumed (`tests/live_calendar_write.rs::partial_update_is_merged_by_the_server`):
    an `update` of `title` alone leaves `uid`, `start`, `timeZone`, `duration` and the rest
    untouched. So there is **no JSCalendar serializer and no document patcher here**, and
    none should be added: the "round-trip from raw plus targeted patches" invariant
    (`calendar-semantics.md`) is satisfied by the *server*. This is the mirror image of
    CalDAV, whose `PUT` replaces the whole resource and therefore forces the client to do
    fold-aware content-line surgery over the stored `RawIcal` — which is exactly why
    `patch_event_ical` stays in `provider-caldav` while only the *intent*
    (`EventPatch`/`PatchTarget`) is neutral.
  - **Time model on the way out.** `start` is the wall clock (`LocalDateTime`, no offset)
    and the zone is a separate `timeZone`, so a move rewrites `start` and **never**
    `timeZone` — writing the UTC instant instead would move the event for every reader in
    another zone and re-time the series at the next DST boundary. A form change is
    *rejected*, not converted (`CalendarDateTime::has_same_form`). All-day is
    `showWithoutTime: true` with a null zone; JSCalendar has no end, so an end edit is
    re-derived as a `duration` from the start the event will have.
  - **One occurrence** is `recurrenceOverrides/<original start>/…` (RFC 8984 §4.3.3) — the
    server materializes the override from the series itself, so `PatchTarget::Instance`
    needs none of the start/end CalDAV requires to split a `RECURRENCE-ID` `VEVENT` by hand.
  - **Locations are a map, not a scalar.** JSCalendar `locations` is `Id[Location]`, so
    renaming "the location" patches `locations/<its id>/name` — keeping that location's
    coordinates and any others. The id lives *only* in the preserved `RawJsCalendar`, which
    is what the read path's raw is for on the write side. A **create** has no such id yet, so
    it mints the sole entry at a fixed id (`{"1": {"@type": "Location", "name": …}}`) — the
    same id a later location edit reuses when it finds the event already has one, and the
    entry `parse_locations` reads straight back into the projection.
  - **A destroy of an already-gone event is success**, not a `notFound` error: the desired
    end state holds, which is what makes an outbox retry of a delete whose response was lost
    safe (the same contract CalDAV gets from treating `404`/`410` as success).
  - **`updated: {id: null}` is an acknowledgement**, and a target the server mentions in
    *neither* the applied nor the failed map is a synthetic `notFound` conflict — never a
    silent success. (Both hard-won on the `Email/set` path; see **Mail writes**.)
  - There is **no whole-document write verb**: a JSCalendar object is not a file whose bytes
    the client owns, so `Provider::put_event` stays unimplemented here even though the
    adapter advertises `calendar_writes` — that capability covers the neutral spine, not the
    document escape hatch that exists for CalDAV's iMIP RSVP primitive.
  - **A `/set` returns no object, so the write reconciles through the delta** (issue #65).
    `created` carries an id and `updated` an acknowledgement — never the stored event — so
    the store would otherwise keep the pre-write copy. The facade follows every calendar
    write with `engine_sync::reconcile_calendar_events`, which here is one
    `CalendarEvent/changes` + back-referenced `/get`: exactly the read primitive, one round
    trip. That a server re-delivers **our own** write on that delta (and reports our own
    destroy in `destroyed`, and advances `state` so it is not re-delivered forever) is the
    fix's load-bearing assumption and cannot be tested offline — the fake executor would
    "confirm" any delta at all — so `tests/live_calendar_reconcile.rs` pins it against
    Stalwart. There is no `ETag` chain to refresh here (a `CalendarEvent` has no per-object
    revision), so what the reconcile buys on this transport is simply that the store's copy
    still comes from the **server**.
- **There is no lost-update guard on JMAP calendar writes, and the engine says so.**
  `Capabilities::calendar_write_guard()` returns `WriteGuard::Absent` (CalDAV returns
  `Enforced`), so a host reads the truth **before** it writes rather than inferring optimistic
  concurrency that is not there. Two independent reasons, both established rather than
  assumed:
  1. **A `CalendarEvent` carries no per-object revision.** No `ETag`, no `changeKey` —
     `RevisionTokens` is empty for every JMAP object by construction. There is nothing to
     name *this* event's version with.
  2. **`ifInState` is the wrong instrument, not merely a broken one.** RFC 8620 §5.3 scopes
     it to the account's whole `CalendarEvent` **type state**, not to the object. On a
     *spec-compliant* server, guarding an edit of one event with it means the write is
     rejected because somebody added an **unrelated** meeting since our last sync — a
     spurious failure, not lost-update protection. And its value would have to be the sync
     cursor, which is a property of the sync, not of the event being written.

  Stalwart's handling of `ifInState` **changed under us**, and neither state changes the
  decision. v0.16.11–v0.16.13 parsed it and never compared it (a stale-state `/set` was applied
  and returned a fresh `newState`, where RFC 8620 §5.3 requires a `stateMismatch`; a *malformed*
  state string still `400`s, so it was parsed, just never checked). v0.16.14 fixed it, and the
  harness now pins **v0.16.15**, so enforcement is what our live runs meet: a stale-but-
  well-formed token is refused with `stateMismatch` and the write does not land. That only
  sharpens reason 2 — the probe's state had moved because of an edit to a *different* property
  of a *different* event, which is precisely the spurious rejection a per-event guard must not
  produce.

  **So we send no `ifInState`**, on either vintage. Instead the absence of the guard is
  *asserted live* (`a_stale_edit_is_not_refused`): a write built on a superseded copy lands, and
  the concurrent edit is silently lost.

  Be precise about what that test can and cannot catch. It drives the **adapter**, which sends
  no precondition, so server-side `ifInState` enforcement can never fail it — and did not: it
  passes unchanged on v0.16.15. It is a tripwire for *the writes we actually send* losing their
  ability to clobber, which is what `WriteGuard::Absent` claims; it is **not** a tripwire for
  Stalwart gaining a precondition. A host that must not lose a concurrent edit has to detect it
  above the engine. The one thing that must not happen is a neutral write API that *looks* like
  it gives optimistic concurrency on every provider when here it gives none.

  **Reason 2 is demonstrated, not merely argued** —
  `provider-jmap/tests/live_calendar_precondition.rs` sends `ifInState` over the harness's raw
  JMAP seam (the adapter cannot, by design) and shows the real server refusing a write to an
  event **nobody touched**, with `stateMismatch`, because a *different* property of a
  *different* event changed. That is the spurious rejection, against a server implementing
  §5.3 correctly. Two details worth carrying: the refusal is a **top-level method error**, not
  a per-object `notUpdated`, so one unrelated change loses every write in a batched `/set`;
  and a *malformed* token is a `400 notRequest` instead, so "a bad token fails" is **not**
  evidence of enforcement — check *which* error you got before concluding anything.
- **Scheduling is opt-in, and omitting the flag fails silently.** `sendSchedulingMessages`
  (draft-ietf-jmap-calendars §5.3, default **`false`**) is what makes the server derive the
  iTIP message from a `/set`. A request without it is stored and notifies **nobody**: the
  organizer is never told an invitation was answered, and an attendee keeps a meeting the
  organizer cancelled. Nothing in the response distinguishes that from success.

  The adapter sends it on all four verbs. The RSVP carries the caller's choice
  (`EventRsvp::notify_organizer`); create/patch/destroy send `true` unconditionally
  (`calendar_write::SCHEDULE`), because the neutral write verbs carry no notify control for a
  caller to state and the transports that do schedule — CalDAV's RFC 6638 auto-schedule, and
  Graph — do it unconditionally, so the engine's answer to "does a calendar write reach its
  participants?" does not depend on which transport is underneath.

  This is also why JMAP is the **one** server-scheduled-looking transport that advertises
  `RsvpControls::suppress_notification: true`: the notification is a field of the request
  here, not something the server does on its own. It read `false` until #102 — which was not
  a judgement about JMAP but a description of an adapter that never sent the argument at all,
  and so advertised that it could not suppress the reply while in fact suppressing every one.

  **The trap this came out of is worth more than the fix.** The live suite asserted the
  organizer was never told, and blamed Stalwart, for months — because it only ever sent the
  one request shape the adapter builds, and the CalDAV control arm "worked" (auto-schedule,
  no equivalent opt-in, never comparable). A live server does not rescue you from the
  `AGENTS.md` fake-request-shape trap if you only send one shape. Both directions are now
  pinned (`tests/live_calendar_scheduling.rs`), and each was verified to fail with the flag
  removed.

## Known limitations (documented, not bugs)

- **JMAP cannot send an iMIP scheduling message, and the adapter refuses rather than
  trying** (issue #105). `Capabilities::scheduling_submission` is `false` here and `true`
  on the three transports that submit assembled RFC 5322 bytes.

  RFC 6047 §2.4 requires a `method=` parameter on the part's `Content-Type`, and §2.4
  note 2 says a `text/calendar` part *without* one is not an iMIP body part at all — it
  arrives as a calendar file the organizer's client never processes. An `EmailBodyPart`'s
  `type` is a media type **without parameters**, so it cannot carry it. RFC 8621 §4.1.3's
  raw `header:Content-Type` looks like the way round, and §4.6 explicitly permits
  `Content-*` fields on a body part (forbidding them only on the `Email`).

  Driven against Stalwart, all three shapes fail, and **all three send successfully** —
  which is why this is a refusal and not a best-effort:

  | `EmailBodyPart` shape | what the server stores |
  | --- | --- |
  | `header:Content-Type` alone | **two** `Content-Type` fields — ours, then a generated `text/plain`. A parser taking the last instance sees plain text. |
  | `type` + `header:Content-Type` | two again — ours, then a generated `text/calendar` with no `method=`. Also breaks §4.6's "MUST NOT be two properties that represent the same header field". |
  | `type` alone | one clean field, and no `method=` anywhere. |

  Pinned in `tests/live_imip.rs`, which issues the three shapes over **raw JMAP**
  (`Harness::jmap_post`) precisely because the adapter has no code path that builds them —
  and which asks for `header:Content-Type:all`, the array form, because the singular form
  returns only the last instance and would hide the duplicate entirely.

  This is a **server** limitation, not necessarily a protocol one: a JMAP server that
  emitted only the client's field would work. But Stalwart is the only JMAP mail server
  this repo can drive (see the Fastmail note above), so shipping a path that is malformed
  on the only server we can verify is worse than refusing. If that test ever goes red
  because a server behaves better, the refusal should be lifted.

- **Raw MIME is fetched on demand, not synced.** Sync ships Tier-1 metadata only;
  the raw RFC 5322 source is downloaded lazily via the `blobId` when a host opens a
  message (`fetch_message_source`, above) and cached by the store thereafter.
  Eager/durable raw-MIME storage *at sync time* is still a later store sub-step.
  Calendar raw (`RawJsCalendar`) *is* preserved (it is a serde field on the object).
- **JMAP calendar writes have no lost-update guard.** See the calendar-writes section
  above: the transport cannot refuse a stale edit, so last-writer-wins is the real
  semantics. This is reported honestly (`WriteGuard::Absent`) rather than papered over,
  and it is a *documented property of the transport*, not a defect in the adapter.
- **A `participationComment` is not carried.** The RSVP itself ships (see **Scheduling is
  opt-in** above), but RFC 8984's `participationComment` is advertised as absent
  (`RsvpControls::comment: false`) and a caller that supplies one is refused: no server we
  run is known to relay it into the `REPLY`, and a note that may go nowhere is worse than one
  never offered. Verify against a server before promising it.
- **A neutral `EventDraft` cannot state a recurrence rule.** Both adapters share this gap
  (CalDAV's `build_event_ical` cannot either), so a recurring event can only be *created*
  through the CalDAV whole-document verb today. Editing one already exists on both. It is
  the obvious next extension of the draft, and the live `recurrence_override_edit` test
  works around it by editing the seeded series and restoring it.
- **Calendar events are still fetched whole**, not streamed: only email has a
  streaming primitive (`stream_email`) so far. Events have no natural recency sort and
  the seed fits one page; when streaming is wanted there, generalize `member_page` with
  a per-type sort and add an event stream. Snapshot-during-mutation across pages
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
  and records the upload URL/type/bytes).
  - **The fake records the requests it was sent** (`FakeExecutor::requests`/`sole_call`).
    It replies with canned bytes *whatever* it receives, so on its own it cannot catch a
    wrong request **shape** — a `CalendarEvent/set` with a bad JSON pointer or a malformed
    JSCalendar object would sail through (`AGENTS.md`). Recording the exact
    `{using, methodCalls}` envelope closes that gap for shape, and the
    `CalendarEvent/set` tests (`calendar_write_tests.rs`) assert the produced JSON
    *literally* — the create's object, the update's pointers, the
    `recurrenceOverrides/<start>/…` prefix, the `null`-removes-a-property patch. What
    offline still cannot prove is that a real server **accepts** it; that is the live suite,
    and it is not optional for a write.
  The **multipart body-structure** assembly is
  unit-tested directly (`submit_body`), and the **EventSource watcher** is driven over
  a **scripted `ChunkSource`** — SSE frame parsing (split chunks, CRLF, comments,
  multi-`data`), `StateChange` classification, watched-type filtering, and the
  `Changed`/`KeepAlive`/closed-stream event loop — all offline. A **blocking mock HTTP
  server** exercises the real transport, session discovery, and `execute`. In
  `engine-sync`, a store-probing fake proves each streamed chunk is committed and
  host-visible before the next is fetched, a recording `SyncObserver` checks the
  `fetched`/`total` sequence and the upserted rows, and a lease-stealer proves a
  mid-stream `StaleLease` restarts safely (the checkpointed/held cursor makes it
  idempotent). A panic-resistance test
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
  all live files are excluded from the offline coverage metric, like the harness probes.
- **Live calendar writes** (`tests/live_calendar_write.rs`) mirror the CalDAV write suite,
  and two of the four carry the load:
  - `partial_update_is_merged_by_the_server` — the **premise of the adapter**. If the server
    replaced instead of merging, retitling an event would silently wipe its zone, duration
    and recurrence, and *no offline test could see it*, because on this transport we hold no
    document to compare against.
  - `a_stale_edit_is_not_refused` — asserts the **absence** of the lost-update guard, so
    `WriteGuard::Absent` is a measured fact rather than a claim. **If this test ever fails,
    that is good news and a required change**: the server started refusing stale writes, and
    `session.rs` must stop advertising `Absent`.
  - `round_trip` (create → patch → destroy, plus the idempotent re-destroy) and
    `recurrence_override_edit` (is a `recurrenceOverrides/<start>/…` pointer accepted?) cover
    the wire shapes. The recurrence test edits the **seeded** series — the only recurring
    event available, since a neutral draft cannot yet state a rule — and restores it before
    returning, so the seed the read tests assert on is left exactly as found.
- **Fuzzing:** `fuzz/` is a separate cargo-fuzz workspace (`cargo +nightly fuzz
  run jmap_parse`) driving `provider_jmap::fuzz_parse` (behind the `fuzzing`
  feature) over the JSON parse + normalize pipeline.

## JMAP Contacts

RFC 9610's `urn:ietf:params:jmap:contacts` capability supplies a separate primary
account. `AddressBook/get|changes` and `ContactCard/get|changes` map onto normal
container/card scopes while preserving non-empty multi-address-book membership,
JSContact property-map ids, group cards, rights, extensions, and raw JSContact.

`ContactCard/set` implements create/update/destroy. Update is the server's
PatchObject and does not rebuild the stored document. There is no enforceable
per-object revision guard, so contact writes advertise `WriteGuard::Absent`; an
account-wide `ifInState` is not presented as lost-update protection.
Blob-backed media is fetched through the session download template on demand.
For host-facing writes, connect a source adapter per discovered book and
call `JmapProvider::with_contact_address_book` with that opaque id. Until it is
called the adapter advertises **no** destination at all: there is no well-known
default book, and inventing one (the constructor once used the literal
`default`) let a host's create-validation pass and pushed the failure onto the
wire. Programmatic cards and normalized patch values are encoded back to
JSContact names and property-map metadata. A raw-backed create contributes only
what the engine does not model — vendor `x-` extensions, newer JSContact
properties — while every modelled property, including one the host emptied, is
re-derived from the card; returning raw verbatim shipped the pre-edit values of
any card a host cloned and modified.
