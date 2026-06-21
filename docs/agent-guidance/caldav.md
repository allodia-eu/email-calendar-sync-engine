# CalDAV Client Guidance

This document is authoritative for the **CalDAV (RFC 4791) calendar read/sync
**and write** provider** — the calendar half of build-order step 5
(`north-star.md`). It covers the `provider-caldav` crate and the CalDAV/WebDAV
specifics it implements against the Stalwart fixture. Read it before touching
`provider-caldav`, alongside `providers.md` (the Provider Contract),
`store-and-sync.md` (the apply/lease model, the outbox, and `SyncScope`), `jmap.md`
(the calendar-read precedent it mirrors), `calendar-semantics.md` (the time model,
recurrence subset, iTIP/iMIP), and `stalwart-harness.md` (the fixture).

The **IMAP/SMTP mail half** of step 5 is the other slice (`imap-smtp.md`).
**CalDAV writes** (conditional `PUT`/`DELETE` with `If-Match`/`If-None-Match`) are
**implemented** (see "CalDAV writes") and outbox-driven by
`engine_sync::write_calendar_event`/`delete_calendar_event`. **iTIP/iMIP**
inbound parsing + the RSVP write primitive are **implemented** (see "iMIP
scheduling"); the remaining scheduling deferrals (the Scheduling-Inbox `REPORT`,
client-iMIP SMTP delivery, `ClientImip` local-origin persistence) and
**CardDAV/contacts** are out of this slice — see "Known limitations".

## The crate

- **`provider-caldav`** — a CalDAV client over HTTP that implements
  `engine_provider::Provider` for calendar **read/sync and write**. It reuses the
  `Executor`-seam pattern from `provider-jmap`: every request goes through a
  `DavExecutor` trait, so the whole discovery/sync/write orchestration is
  offline-tested by replaying captured Stalwart response documents. The live
  transport is `reqwest` + rustls (pure-Rust TLS, mobile cross-compile), like
  `provider-jmap`. The headline difference from JMAP is that the calendar payload
  arrives as **iCalendar (RFC 5545)**, which this crate parses, where JMAP supplied
  JSCalendar directly — so the bulk of the crate is an iCalendar parser producing
  the **same** normalized [`Event`]/[`Calendar`] projection the JMAP adapter does.
- Layers: `ical` (the RFC 5545 parser: `unfold` → `component` tree → `value`/
  `recur`/`party`/`event` normalizers → one folded `Event` per resource), `dav`
  (the WebDAV `multistatus` XML parser, via `quick-xml`), `transport` (the
  `DavExecutor` seam — read reports plus the `send_write` write verb — + its
  `reqwest` implementation), `request` (the PROPFIND/REPORT bodies),
  `discovery`/`calendar` (principal → home → collection listing), `sync` (the
  `sync-collection` REPORT snapshot/delta logic), `write` (the conditional
  `PUT`/`DELETE` of event resources), `provider` (the `Provider` impl).

## How CalDAV differs from JMAP (the shape)

- **Calendar payload is iCalendar, not JSCalendar.** A CalDAV calendar object
  resource is one `text/calendar` document; the crate parses it into the engine's
  JSCalendar-shaped `Event`. The original text is preserved verbatim as `RawIcal`
  beside the lossy projection (model invariant). Enum spellings that differ
  between iCalendar and JSCalendar are mapped explicitly (`STATUS`/`TRANSP`/
  `CLASS`/`ROLE`/`PARTSTAT`), not by lowercasing.
- **A resource folds master + overrides into one event.** All `VEVENT`s in a
  resource share one `UID` (RFC 4791 §4.1): a series **master** plus its
  `RECURRENCE-ID` overrides. The parser folds them into a *single* `Event` — the
  master carrying its overrides inline in `recurrence.overrides` (an `EXDATE`
  becomes an `Excluded` override; a `RECURRENCE-ID` `VEVENT` becomes a `Patch`
  carrying its moved `start`/`duration`/`title`) — exactly the shape one JMAP
  `CalendarEvent` produces, so the recurrence expander and the rest of the engine
  see one representation regardless of transport. A resource with only an override
  (no master) yields that override as a standalone instance event
  (`calendar-semantics.md`).
- **Calendar scope is per collection.** Like IMAP's per-mailbox email, CalDAV
  state is per collection (a sync-token, RFC 6578). So a `CalDavProvider` is
  **bound to one calendar collection** for events: `event_scope` is
  `SyncScope::DavCollection{account, collection}` (the collection href), and
  `sync_events` is a `sync-collection` REPORT over it. The account's **calendar
  list** syncs under the new per-account container scope
  `SyncScope::DavCollectionList{account}` — a `PROPFIND` of the calendar home
  re-snapshots it each pass (no list cursor), applied before the per-collection
  events it parents (`store-and-sync.md` referential apply order). This mirrors
  IMAP's `ImapMailboxList` → `ImapMailbox` exactly. The cross-collection fan-out
  (drive every calendar) is the later orchestrator's job.
- **Identity is the resource href.** An event's `EventId` is its resource href
  (URL-encoded, as the server returns it); the iCalendar `UID` is the separate
  cross-system `Uid`. The `getetag` is preserved in `event.revisions` (the `ETag`
  for a future `If-Match` write). The `DavCollectionId` (the scope's collection
  key) and the `CalendarId` (event membership) both wrap the collection href.
- **Calendar capabilities only (read + write), no mail.** A `CalDavProvider`
  advertises `Capabilities::calendars` **and** `Capabilities::calendar_writes` —
  it reads/syncs and writes over the same HTTP transport — and does no mail. The
  write capability is **separate** from the read one (mirroring `submission` being
  separate from `mail`), so a read-only calendar — a shared CalDAV collection the
  account cannot write, or a future calendar-read-only adapter — advertises
  `calendars` without `calendar_writes`; callers route a write by capability, never
  by provider kind. To support a calendar-only provider cleanly, the `Provider`
  trait's mail methods (`sync_mailboxes`/`sync_email_page` and the
  `mailbox_scope`/`email_scope` accessors) are **default-able** (unsupported /
  JMAP-default), symmetric with how the calendar methods already defaulted for a
  mail-only provider; the JMAP and IMAP adapters still override them.

## CalDAV specifics implemented

- **Discovery is the two-step RFC 6764 §6 flow.** `PROPFIND` the well-known path
  (`/.well-known/caldav`) for the `current-user-principal`, then `PROPFIND` *that
  principal* for its `calendar-home-set` (the home-set is a property of the
  principal, not the root). A lenient server (Stalwart) returns the home-set
  directly at the well-known, short-circuiting the second step; a strict server
  (SabreDAV/Soverin) returns only the principal there, so the second `PROPFIND` is
  required — skipping it fails with "no calendar-home-set". Each `PROPFIND` follows
  the server's redirect itself (the transport does **not** auto-follow, mirroring
  the JMAP session flow). Then `PROPFIND Depth:1` the home and keep the responses
  whose `resourcetype` marks them a `calendar`. Hrefs may be absolute paths or full
  URLs; the executor resolves them against the connection origin (the JMAP
  `RebaseToConnection` posture), and a bound-collection value that is itself an
  absolute path or full URL (a discovered calendar href) is used verbatim.
- **Event sync is one `sync-collection` REPORT (RFC 6578).** It is the whole
  primitive: an **empty** prior token returns every resource — a **snapshot**
  whose accumulated `present` set tombstones anything absent — while a **held**
  token returns only the changed (`2xx`, carrying inline `calendar-data`) and
  removed (a response-level `404`) resources — a **delta**. Either way the response
  carries the next `sync-token`, which becomes the opaque cursor. No separate
  `calendar-query`/`calendar-multiget` round trip: requesting `<C:calendar-data/>`
  in the REPORT returns each resource's iCalendar inline.
- **Self-healing invalid token.** A server that rejects a stale token (RFC 6578
  §3.2 `valid-sync-token`, a `403`/`409` precondition) is recovered by re-running
  the REPORT with an empty token — a snapshot — **inside the adapter**, the same
  way the JMAP adapter recovers from `cannotCalculateChanges`. The orchestrator
  never sees it.
- **WebDAV XML is prefix-agnostic.** Servers choose their own namespace prefixes
  and return absent properties in a separate `404` `propstat`; the parser matches
  on **local element names** and keeps only `2xx` `propstat` properties. CDATA
  (the `calendar-data` payload) and entity-escaped text are handled by `quick-xml`.
  A document truncated mid-stream (elements still open at EOF) is a hard error, so
  a short snapshot can never wrongly tombstone resources.
- **Time model.** `DTSTART` + `TZID`/`Z`/neither → zoned/UTC/floating, and a
  `VALUE=DATE` (or bare 8-digit) value → all-day, all mapped to the engine's
  four-case `CalendarDateTime`. The length is `DTEND − DTSTART` (a new
  `CalendarDateTime::duration_until` in `engine-core`, splitting the span into
  nominal days + the absolute remainder per RFC 5545 §3.6.1) or an explicit
  `DURATION`; a `DATE` start with neither defaults to one day, a `DATE-TIME` start
  to zero. A `TZID` is taken as an IANA zone (the seed + near-universal case; the
  embedded `VTIMEZONE` is preserved in `RawIcal`).
- **Hardened parsing.** Content-line unfolding, quote-aware param splitting (a
  `:`/`;` inside a quoted param value is not a delimiter), TEXT unescaping, and a
  tolerant `BEGIN`/`END` component tree (loose properties, stray `END`s, and
  unclosed components degrade gracefully). Every parse path returns an error
  rather than panicking on hostile input; a single malformed resource is **skipped**,
  never failing the whole sync pass.

## CalDAV writes

- **Create/update is one conditional `PUT`; delete is one `DELETE`** (RFC 4791
  §5.3.2). The `write` layer builds the request and maps the response to a receipt
  or a classified error; the live `DavClient::send_write` carries a typed body and
  the conditional header, distinct from the read `send` (Depth + XML) — so the
  proven read path is untouched. The verbs live on the `Provider` trait as
  `put_event`/`delete_event`, default-rejecting (unsupported) so a
  capability-checking caller never relies on them; `CalDavProvider` overrides both
  and advertises `Capabilities::calendar_writes`.
- **Optimistic concurrency rides on the `ETag`.** A create sends `If-None-Match: *`
  (never overwrite an existing resource at the href); an update or guarded delete
  sends `If-Match: "<etag>"` (apply only while the server copy is unchanged). A
  failed precondition is `412` → `FailureClass::Conflict`, recovered by refetch and
  merge, **never a blind retry** (`error.rs`). `PUT` and `DELETE` are **idempotent
  HTTP methods** (RFC 7231 §4.2.2), and the precondition makes a retry
  self-correcting: a retried create `412`s if the first landed, a retried update
  `412`s once the ETag moved, and a retried delete sees the resource already gone.
  So a lost-response retry is **safe** — there is no ambiguous `NeedsConfirmation`
  case as there is for SMTP. The read slice already preserves each event's `ETag` in
  `event.revisions`, so a write `If-Match`es without a refetch (the deferred read
  promise, now fulfilled).
- **`DELETE` is idempotent: already-gone is success.** A `DELETE` whose resource is
  **already absent** (`404`/`410`) resolves as `Ok` (RFC 7231 §4.3.5), not a
  `Permanent` error — so re-running a delete whose response was lost (the first one
  landed) succeeds rather than reporting a spurious failure. A `412` (the resource
  still exists but its ETag moved) remains a genuine `If-Match` conflict, surfaced
  for refetch.
- **The body is the round-tripped `RawIcal`, never a re-serialized projection**
  (`calendar-semantics.md`, `modeling.md`): an update PUTs the stored `raw_ical`
  with targeted patches applied, so properties the lossy JSCalendar projection
  cannot express (`X-` props, `VALARM`, …) survive the round trip (locked by an
  offline test). A **create** carries a freshly built iCalendar document — the
  body is constructed by the host/caller, since this slice is the transport +
  outbox primitive, not a JSCalendar→iCalendar serializer (that, and a structural
  iCal patcher for updates, are separate concerns). `CalDavProvider::event_href`
  mints the conventional `<collection>/<uid>.ics` resource href for a create
  (percent-encoding the `UID` as one path segment); an update/delete reuses the
  stored `EventId`.
- **The new `ETag` is read back where the server supplies it.** A successful PUT
  returns the resource's new entity tag in the `ETag` response header (RFC 4791
  §5.3.4), surfaced on the receipt; when the server omits it the receipt carries
  `None` and the next `sync-collection` delta refreshes `event.revisions`. No
  automatic follow-up `GET` is issued.
- **Writes are outbox-mediated** (`store-and-sync.md` Write Contract). The thin
  drivers `engine_sync::write_calendar_event`/`delete_calendar_event` mirror
  `submit_mail`: a durable `PendingOp` (serialized on the resource href so writes
  to one event never race) is recorded **before** the side effect, claimed under a
  fenced `OpLease`, and resolved `Succeeded`/`Failed` under that lease. The
  **idempotency key is a caller-supplied argument**, not derived from the href: the
  store dedups enqueue by `(account, idempotency_key)` across *every* op state
  (including terminal), so a href-only key would wrongly collapse two distinct edits
  of one resource into one op. The host mints a key per write intent.

## iMIP scheduling

The `imip` module is the iMIP (iTIP over email, RFC 6047) surface; the pure
decision/trust/apply logic lives in `engine_core::scheduling`, and
`calendar-semantics.md` is authoritative for the inbound-scheduling design.

- **Parse.** `imip::parse(text)` reuses the iCalendar parser (`ical`) to turn a
  `text/calendar` body into an `engine_core::scheduling::SchedulingMessage` — the
  `VCALENDAR` `METHOD`, the folded `VEVENT` `Event` projection, and the `DTSTAMP`.
  Absent `METHOD` is an error (it is then a stored object, not a scheduling
  message). A parsed message's `EventId`/`CalendarId` are **synthetic placeholders**
  minted from the `UID` (`imip:<uid>` / `imip:scheduling`); an iMIP body has no
  provider href/collection, and reconciliation keys on `(UID, SEQUENCE,
  RECURRENCE-ID)` regardless, so storage identity is assigned only when the event is
  stored. The shared fold logic (`resource_components`/`fold_overrides`) is factored
  out of `parse_calendar_object`, so the read path and the scheduling path produce
  the *same* `Event`.
- **RSVP write primitive.** `imip::set_my_partstat(stored_raw, me, status)` patches
  *my* `PARTSTAT` into a stored event's raw iCalendar and returns the body to `PUT`
  back. It is a **targeted raw edit**, not a re-serialization: untouched physical
  lines (other attendees, the organizer, `X-` properties, `VALARM`s) re-emit
  byte-for-byte, only the matching `ATTENDEE` line's `PARTSTAT` changes (an absent
  one is appended), and the rewritten line is re-folded to ≤75 octets (RFC 5545
  §3.1). The engine `ParticipationStatus` (lowercase JSCalendar spelling) is mapped
  back to the uppercase iCalendar `PARTSTAT` token. The result feeds
  `EventWrite::update`/`If-Match` through the existing
  `engine_sync::write_calendar_event` outbox driver — **no new write verb or outbox
  op**. On a CalDAV auto-schedule server (RFC 6638) the changed `PARTSTAT` is what
  the server turns into the iTIP `REPLY` to the organizer.

## Known limitations (documented, not bugs)

- **iTIP/iMIP inbound parse + RSVP are implemented; delivery/persistence wiring is
  staged.** `engine_core::scheduling` (keys, `SEQUENCE` ordering, the trust
  decision, `reconcile` → `ScheduleAction`, the `apply_reply`/`cancel` event
  mutations) and `provider_caldav::imip` (parse + `set_my_partstat`) are done and
  offline-tested end to end through the conditional-`PUT` outbox driver. Still
  deferred (`calendar-semantics.md`): the **mail-sync wiring** that fetches the
  detected `text/calendar` part's bytes and drives `reconcile`; the **CalDAV
  Scheduling Inbox** `REPORT` (RFC 6638) and a live Stalwart scheduling test;
  **client-iMIP `REPLY` delivery** over SMTP (the assembler is `text/plain`-only);
  and **`ClientImip` local-origin persistence** (storing a brand-new inbound
  `REQUEST` has no provider-less single-event store path yet).
- **Only event object resources are written, not collections.** Creating or
  deleting a *calendar collection* (`MKCALENDAR`, RFC 4791 §5.3.1; collection
  `DELETE`) is out of scope — the write slice manages event resources within an
  existing collection. The host provisions calendars out of band.
- **A JSCalendar→iCalendar serializer and a structural iCal patcher are separate
  concerns.** The write carries the iCalendar body as `RawIcal`; constructing it
  for a create, and applying targeted patches to the stored raw for an update, are
  the caller's job in this slice (the engine supplies the conditional-`PUT`
  transport + outbox, not the serialization). The lossy projection is never
  re-serialized to the wire.
- **CardDAV/contacts are out of scope.** Contacts land after step 5
  (`north-star.md`); the `DavCollectionList`/`DavCollection` scopes and the
  WebDAV/multistatus machinery are already shaped to serve an address-book home
  without rework.
- **Custom (non-IANA) `VTIMEZONE` expansion is staged.** A `TZID` is resolved as
  an IANA zone; a genuinely custom embedded `VTIMEZONE` is preserved in `RawIcal`
  but not parsed into the expander, so such an event stores with no occurrences
  (the staged behavior `calendar-semantics.md` describes for embedded zones).
  Recording which source was used, and the disagrees-with-IANA fixture, ride with
  that slice.
- **`RRULE UNTIL` with a `Z` (UTC) bound** is read as its wall-clock value;
  converting it to the event's zone needs tzdata and is staged. The supported seed
  uses `COUNT`, not `UNTIL`.
- **No CTag fallback yet.** Sync uses the RFC 6578 sync-token (which Stalwart and
  modern servers support). A server with no `sync-collection` support would need
  the CTag-+-per-resource-ETag diffing path (`providers.md`); it is not yet
  implemented.
- **Calendar events are fetched whole, not paged** — consistent with the JMAP
  calendar slice (events have no natural recency sort, and the REPORT returns the
  collection in one pass).

## Testing

- **Offline (always green, no Docker):** the iCalendar parser, the WebDAV
  multistatus parser, the normalizers, and the cursor/snapshot/delta logic are
  unit-tested, including an adversarial panic-resistance pass over hostile
  iCalendar (the `fuzz/` `caldav_parse` cargo-fuzz counterpart). Captured,
  secret-free Stalwart transcripts (`tests/fixtures/`) drive the `dav`/`discovery`/
  `calendar`/`sync` layers through a **fake `DavExecutor`**. A **full offline sync
  loop** (`provider_tests.rs`) drives the real `CalDavProvider` over the fake
  executor through `engine_sync::sync_calendar` into a real `SqliteStore`,
  asserting the six seed fixtures normalize, the master+override folds with its
  `EXDATE` exclusion, participants merge, and occurrences materialize (the weekly
  series → 7, twelve in total). The **write** path is unit-tested through the same
  fake: create (`If-None-Match`)/update (`If-Match`)/delete request-shaping and
  response→receipt mapping (`write.rs`), the `412`→`Conflict` precondition failure,
  the missing-response-`ETag` case, the `event_href` minting + percent-encoding,
  and — the model invariant — that an update **round-trips the preserved `raw_ical`**
  so an `X-` property and a `VALARM` the projection cannot express survive on the
  wire. The outbox drivers are tested in `engine-sync` (a real `SqliteStore`):
  enqueue→claim→`PUT`/`DELETE`→record `Succeeded`, a `Conflict` recorded `Failed`
  without blind retry, and that two distinct edits of one href with **distinct
  idempotency keys both run** (the key-as-argument rationale). The **iMIP** layer is
  unit-tested in `imip.rs` (parse of `REQUEST`/`REPLY`/`CANCEL`, the no-`METHOD` and
  missing-`DTSTAMP` rejections, and the `set_my_partstat` patch — folded input,
  quoted params, bare-LF, an absent/added `PARTSTAT`, the round-trip preserving
  `X-`/`VALARM`, and the case-/scheme-insensitive match), and an **end-to-end RSVP
  flow** in `provider_tests.rs` drives parse → `reconcile` (trusted) → `set_my_partstat`
  → `EventWrite::update` → `engine_sync::write_calendar_event` into a real
  `SqliteStore` over the fake executor (asserting the `If-Match` `PUT` carries my
  accepted `PARTSTAT` and no transit-only `METHOD`), plus the security case that a
  parsed `REQUEST` whose `ORGANIZER` mismatches the authenticated sender is rejected,
  not written.
- **Live against Stalwart (gated on `STALWART_HTTP_ADDR`, skips otherwise):**
  `tests/live_caldav.rs` connects to the real Stalwart over HTTP, runs discovery +
  `sync-collection`, and asserts the same seed invariants in the store, plus an
  idempotent **empty delta** on a second sync (the held sync-token). A second test,
  `caldav_write_round_trip`, drives the full **write lifecycle** against the real
  server — create → update (`If-Match`) → delete (`If-Match`), verified by
  re-reading the collection — and leaves the seed untouched. The two tests
  **serialize** on a shared guard (the write test transiently adds an event, which
  must not race the exact-count assertion). It reuses `crates/stalwart-harness`. The
  `stalwart` CI job runs it; the file is excluded from the offline coverage metric,
  like the JMAP/IMAP live tests.
- **Live against SabreDAV (gated on `SABREDAV_HTTP_ADDR`, skips otherwise):**
  `tests/live_sabredav.rs` runs the **same** seed assertions **and the same write
  round-trip** (`tests/common/`, shared by both live files) against a second,
  independent CalDAV implementation — the SabreDAV fixture (`docker/sabredav/`,
  the stack Soverin/Fastmail-style providers run). Passing here proves the client
  is not over-fit to Stalwart: SabreDAV exercises the **two-step RFC 6764
  discovery**, the `http://sabre.io/ns/sync/N` sync-token form, and its own
  `ETag`/`If-Match` write semantics. The separate `sabredav` CI job runs it; the
  file is likewise excluded from the offline coverage metric. The fixture reuses the
  shared `docker/stalwart/seed/calendar` dataset, so one set of fixtures validates
  both servers.
- **Fuzzing:** `fuzz/` (a separate cargo-fuzz workspace) gained
  `cargo +nightly fuzz run caldav_parse`, driving `provider_caldav::fuzz_parse`
  (behind the `fuzzing` feature) over the unfold → component → normalize pipeline.
- **Real-provider exploration:** `examples/caldav_explore.rs` connects to a *real*
  CalDAV server (Fastmail/iCloud/Google over verifying HTTPS, or a local server
  over HTTP), discovers the calendar home, lists calendars, and prints the bound
  calendar's events (start, kind, title) — read-only by default. Set `CALDAV_WRITE=1`
  to also run a **write demo** that creates a throwaway event and deletes it again
  (the opt-in parallel to `imap_explore`'s `IMAP_DRAFT`/`IMAP_SEND`). It is the
  calendar parallel to `provider-imap`'s `imap_explore`; point it at the local
  Stalwart harness with `CALDAV_URL=http://127.0.0.1:18080`.
