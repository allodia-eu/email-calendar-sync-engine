# CalDAV Client Guidance

This document is authoritative for the **CalDAV (RFC 4791) calendar read/sync
provider** — the calendar half of build-order step 5 (`north-star.md`). It covers
the `provider-caldav` crate and the CalDAV/WebDAV specifics it implements against
the Stalwart fixture. Read it before touching `provider-caldav`, alongside
`providers.md` (the Provider Contract), `store-and-sync.md` (the apply/lease model
and `SyncScope`), `jmap.md` (the calendar-read precedent it mirrors),
`calendar-semantics.md` (the time model, recurrence subset, iTIP/iMIP), and
`stalwart-harness.md` (the fixture).

The **IMAP/SMTP mail half** of step 5 is the other slice (`imap-smtp.md`).
CalDAV **writes** (PUT/`If-Match`/DELETE), **iTIP/iMIP** scheduling, and
**CardDAV/contacts** are explicitly **not** in this slice — see "Known
limitations".

## The crate

- **`provider-caldav`** — a CalDAV client over HTTP that implements
  `engine_provider::Provider` for calendar **read/sync only**. It reuses the
  `Executor`-seam pattern from `provider-jmap`: every request goes through a
  `DavExecutor` trait, so the whole discovery/sync orchestration is offline-tested
  by replaying captured Stalwart response documents. The live transport is
  `reqwest` + rustls (pure-Rust TLS, mobile cross-compile), like `provider-jmap`.
  The headline difference from JMAP is that the calendar payload arrives as
  **iCalendar (RFC 5545)**, which this crate parses, where JMAP supplied
  JSCalendar directly — so the bulk of the crate is an iCalendar parser producing
  the **same** normalized [`Event`]/[`Calendar`] projection the JMAP adapter does.
- Layers: `ical` (the RFC 5545 parser: `unfold` → `component` tree → `value`/
  `recur`/`party`/`event` normalizers → one folded `Event` per resource), `dav`
  (the WebDAV `multistatus` XML parser, via `quick-xml`), `transport` (the
  `DavExecutor` seam + its `reqwest` implementation), `request` (the
  PROPFIND/REPORT bodies), `discovery`/`calendar` (principal → home → collection
  listing), `sync` (the `sync-collection` REPORT snapshot/delta logic), `provider`
  (the `Provider` impl).

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
- **`Capabilities::calendars` only.** A `CalDavProvider` does no mail; it advertises
  only the calendar capability. To support a calendar-only provider cleanly, the
  `Provider` trait's mail methods (`sync_mailboxes`/`sync_email_page` and the
  `mailbox_scope`/`email_scope` accessors) became **default-able** (unsupported /
  JMAP-default), symmetric with how the calendar methods already defaulted for a
  mail-only provider; the JMAP and IMAP adapters still override them. No caller is
  affected because work is routed by capability, never by provider kind.

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

## Known limitations (documented, not bugs)

- **CalDAV writes are out of scope.** This slice is read/sync only. `PUT` with
  `If-Match`/ETag, `DELETE`, and their outbox integration (the Write Contract's
  "CalDAV writes use ETags and `If-Match`") are the next CalDAV slice. The `ETag`
  is already preserved per event so a write slice can `If-Match` without a refetch.
- **iTIP/iMIP scheduling is out of scope.** The model exists in
  `engine_core::scheduling` (keys, `SEQUENCE` ordering, the trust decision), but
  detecting inbound iMIP on the mail path, applying it to stored events, and
  sending replies are a later slice (`calendar-semantics.md`).
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
  series → 7, twelve in total).
- **Live against Stalwart (gated on `STALWART_HTTP_ADDR`, skips otherwise):**
  `tests/live_caldav.rs` connects to the real Stalwart over HTTP, runs discovery +
  `sync-collection`, and asserts the same seed invariants in the store, plus an
  idempotent **empty delta** on a second sync (the held sync-token). It reuses
  `crates/stalwart-harness`. The `stalwart` CI job runs it; the file is excluded
  from the offline coverage metric, like the JMAP/IMAP live tests.
- **Live against SabreDAV (gated on `SABREDAV_HTTP_ADDR`, skips otherwise):**
  `tests/live_sabredav.rs` runs the **same** seed assertions against a second,
  independent CalDAV implementation — the SabreDAV fixture (`docker/sabredav/`,
  the stack Soverin/Fastmail-style providers run). Passing here proves the client
  is not over-fit to Stalwart: SabreDAV exercises the **two-step RFC 6764
  discovery** and the `http://sabre.io/ns/sync/N` sync-token form. The separate
  `sabredav` CI job runs it; the file is likewise excluded from the offline
  coverage metric. The fixture reuses the shared `docker/stalwart/seed/calendar`
  dataset, so one set of fixtures validates both servers.
- **Fuzzing:** `fuzz/` (a separate cargo-fuzz workspace) gained
  `cargo +nightly fuzz run caldav_parse`, driving `provider_caldav::fuzz_parse`
  (behind the `fuzzing` feature) over the unfold → component → normalize pipeline.
- **Real-provider exploration:** `examples/caldav_explore.rs` connects to a *real*
  CalDAV server (Fastmail/iCloud/Google over verifying HTTPS, or a local server
  over HTTP), discovers the calendar home, lists calendars, and prints the bound
  calendar's events (start, kind, title) — read-only. It is the calendar parallel
  to `provider-imap`'s `imap_explore`; point it at the local Stalwart harness with
  `CALDAV_URL=http://127.0.0.1:18080`.
