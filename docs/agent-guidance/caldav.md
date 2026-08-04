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
**CalDAV writes** (the neutral create/patch/delete verbs, rendered as a conditional
`PUT`/`DELETE` with `If-Match`/`If-None-Match`) are **implemented** (see "CalDAV writes")
and outbox-driven by `engine_sync::create_calendar_event`/`patch_calendar_event`/
`delete_calendar_event`. **iTIP/iMIP**
inbound parsing + the RSVP write primitive are **implemented** (see "iMIP
scheduling"); the remaining scheduling deferrals (the Scheduling-Inbox `REPORT`,
client-iMIP SMTP delivery, `ClientImip` local-origin persistence) remain. The
same crate also contains a deliberately separate `CardDavProvider`; calendar and
contact normalization share only the DAV transport/TLS layer.

## CardDAV contacts

`CardDavProvider` starts at `/.well-known/carddav`, resolves the current
principal and `addressbook-home-set`, snapshots address-book collections and
rights, then binds one adapter to one address book. Cards sync with RFC 6578
`sync-collection`; expired tokens restart with a snapshot. Servers without that
report use a CTag check followed by a full `addressbook-query`, retaining each
resource ETag. Direct canonical refetch uses `addressbook-multiget`.

vCard 3/4 input is unfolded and normalized into the JSContact-shaped
`ContactCard` while retaining the complete raw vCard, including malformed legacy
or extension lines the parser does not understand. Targeted writes edit the raw
document and preserve untouched lines. Creates use `If-None-Match: *`; update
and delete require the source ETag under `If-Match`, so the destination
advertises `WriteGuard::Enforced`. Embedded data-URI and authenticated URI photo
reads are supported; photo mutation is not.

Every written value is escaped — `KIND` included, because `ContactKind::Other`
carries host text, and an unescaped line break there injects properties into the
`PUT` body. A name edit writes `FN` **and** `N`: both are stripped before the
replacement is inserted, so emitting only `FN` deletes the structured name from
the server's card. `N`'s nested separators (`;` between slots, `,` within one)
are split escape-aware so the writer and the parser agree.

## The crate

- **`provider-caldav`** — a CalDAV client over HTTP that implements
  `engine_provider::Provider` for calendar **read/sync and write**. It reuses the
  `Executor`-seam pattern from `provider-jmap`: every request goes through a
  `DavExecutor` trait, so the whole discovery/sync/write orchestration is
  offline-tested by replaying captured Stalwart response documents. The live
  transport is `reqwest` + rustls (pure-Rust TLS, mobile cross-compile), like
  `provider-jmap`, built from the shared per-account `TlsClientConfig` in
  `CalDavConfig::tls` (`tls.md`). The headline difference from JMAP is that the calendar payload
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
- **Scheduling support is discovered, not assumed** (issue #105). RFC 4791 is calendar
  *access*; RFC 6638 layers scheduling on top, and §2 makes a conforming server advertise
  the `calendar-auto-schedule` token in the `DAV:` header of an `OPTIONS` response. So
  `connect` asks — one more request in the sequence that already round-trips — and reports
  the answer as `Capabilities::calendar_scheduling`. Without asking, a plain CalDAV server
  looks identical to an auto-scheduling one right up until an RSVP is stored and the
  organizer is never told: `calendar_rsvp` answers *"can this transport express an
  answer?"*, which is a different question.
  - The target is the **calendar home**, not the connection base URL. The header belongs
    to a DAV resource and a server's site root need not be one — Stalwart's answers `302`
    to its web UI with no `DAV:` header at all. The request is a **bare** `OPTIONS`: no
    `Depth`, no `Content-Type`, no body, since the read path's XML framing is meaningless
    on a request with nothing to say (`DavExecutor::send_options`).
  - A response carrying no such token — **whatever its status** — is `false`, not an
    error. A server may answer `OPTIONS` with a `405` and still read and write perfectly;
    failing the connect over one discovery question would refuse a working account. A
    transport failure still propagates, like every other discovery step.
  - Both answers are live-pinned, and the pair is the evidence: **Stalwart advertises it,
    the SabreDAV fixture does not** ("Which server proves what"). The SabreDAV negative is
    a property of that harness's configuration — `docker/sabredav/server.php` loads
    `Sabre\CalDAV\Plugin` and deliberately *not* `Sabre\CalDAV\Schedule\Plugin` — and it
    must stay that way: it is the only server here that can show the capability answering
    `false`, and a capability that came out `true` everywhere would be a constant wearing a
    discovery's clothes.
- **Calendar capabilities only (read + write), no mail.** A `CalDavProvider`
  advertises `Capabilities::calendars` **and** `calendar_writes(WriteGuard::Enforced)` —
  it reads/syncs and writes over the same HTTP transport, and it is the transport that can
  actually promise a lost-update guard (`providers.md`) — and does no mail. The
  write capability is **separate** from the read one (mirroring `submission` being
  separate from `mail`), so a read-only calendar — a shared CalDAV collection the
  account cannot write, or a future calendar-read-only adapter — advertises
  `calendars` without `calendar_writes`; callers route a write by capability, never
  by provider kind. To support a calendar-only provider cleanly, the `Provider`
  trait's mail methods (`sync_mailboxes`/`stream_email` and the
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
  the JMAP session flow), emitting one `ConnectStep::Redirected` per hop to the
  config's `ConnectObserver` (`providers.md`), and `ConnectStep::Discovered` with the
  resolved calendar home once discovery settles. The principal → home-set second step
  is **not** a redirect and emits nothing: it is a second `PROPFIND` of a *different*
  resource, not the same resource moving. CalDAV emits no `Authenticated` step —
  credentials ride on every `PROPFIND`, so there is no discrete authentication
  exchange to observe — and no `TlsEstablished`, because reqwest never exposes the
  negotiated version (`tls.md`). Then `PROPFIND Depth:1` the home and keep the responses
  whose `resourcetype` marks them a `calendar`. Hrefs may be absolute paths or full
  URLs; the executor resolves them against the connection origin (the JMAP
  `RebaseToConnection` posture), and a bound-collection value that is itself an
  absolute path or full URL (a discovered calendar href) is used verbatim.
- **Access rights are asked for, never assumed.** The calendar-list `PROPFIND` requests
  **`DAV:current-user-privilege-set`** (RFC 3744 §5.4) alongside the display metadata —
  one round trip, not a second per collection — and `calendar.rs` maps it onto
  `CalendarAccess`. It has to be *asked*: the privilege set is what the **authenticated
  principal** may do **here**, so a subscribed holiday feed and a colleague's read-only
  share are ordinary calendar collections distinguished only by the privileges they
  grant *this* user. `DAV:all`, `DAV:write` or `DAV:write-content` → `may_write`;
  otherwise `CalendarAccess::reader()`. The predicate itself is
  `Props::grants_member_writes` in `dav.rs`, **shared with the CardDAV
  address-book path**: the spellings are the same RFC 3744 privileges, and two
  copies had already drifted — the address-book copy omitted `DAV:all`, so a book
  reporting `{all, read}` was permanently read-only and every write to a book the
  user owns failed. **`DAV:write-properties` is not enough** —
  SabreDAV grants exactly that on a read-only share (you may rename your copy of it), so
  counting it as a write would reinstate the lie. Only `may_write` is derived: the
  privilege set says nothing standard about whether the *collection* may be deleted
  (that is `DAV:unbind` on the parent home, not on the calendar) or shared, so the other
  flags stay at the `owner()`/`reader()` presets rather than being invented from one
  server's spelling.
  - **A server that reports no privilege set at all is taken as writable** — today's
    optimistic behaviour, now a recorded decision rather than a silent default. RFC 4791
    §2 requires a CalDAV server to support WebDAV ACL, so silence is non-conformance, not
    a considered "no"; and the failure modes are asymmetric — guessing "writable" costs a
    `403` on a write the user chose to attempt, while guessing "read-only" hides the edit
    affordance entirely on a server that works fine. The `403` is the backstop.
    A privilege set that is *present but empty* is a different thing ("you may do nothing
    here") and yields a reader; the parser keeps `None` and `Some(∅)` distinct for
    exactly this reason.
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

- **The write verbs are neutral; the iCalendar is CalDAV's business.** A host states
  *intent* through `engine-provider`'s three verbs — `create_event(&EventDraft)`,
  `patch_event(base: &Event, &EventEdit)`, `delete_event(&EventDeletion)` — and this crate
  renders it. `build_event_ical` (create) and `patch_event_ical` (update) are therefore
  **internal**: a host no longer assembles iCalendar, does not mint an href, and never sees
  an `ETag`. That is what makes the same host code drive JMAP (`jmap.md`), and it is the
  resolution of the "designing a neutral API from one implementer" problem — there are two
  now, and they disagree about everything below the intent.
- **Create/patch is one conditional `PUT`; delete is one `DELETE`** (RFC 4791 §5.3.2).
  CalDAV has **no partial write**, so a patch is *still* a whole-document `PUT` — of the
  stored bytes with the edit applied. The `write` layer builds the request and maps the
  response to a receipt or a classified error; the live `DavClient::send_write` carries a
  typed body and the conditional header, distinct from the read `send` (Depth + XML) — so
  the proven read path is untouched. `CalDavProvider` also implements the whole-document
  escape hatch `put_event`, which exists for the iMIP RSVP primitive alone (below); JMAP has
  no such verb.
- **A document write states a precondition, and "create" is one of them.** `EventWrite.guard`
  is a three-state `WritePrecondition`, and each state has an exact rendering here:
  `IfUnchanged(tokens)` → `If-Match: "<etag>"`, `IfAbsent` → `If-None-Match: *` (RFC 7232
  §3.2), `Unconditional` → no conditional header. `IfAbsent` is what makes **storing an
  inbound invitation** safe: on the very common account shape of IMAP mail plus a CalDAV
  calendar that does no scheduling, an invitation arrives as an iMIP message and nothing
  puts it on the calendar but the host — and it must go in through this verb, not
  `create_event`, because an `EventDraft` carries neither `ORGANIZER` nor `ATTENDEE` and
  would store a plain appointment with nothing to answer on afterwards. The guard matters
  because the concurrent writer is usually the *server*: an auto-scheduling one deposits
  its own copy the moment the organizer writes, and an unconditional `PUT` would erase it
  along with whatever the server had recorded about delivery. A `412` is the same
  `Conflict` as any other precondition failure — re-read and decide, never a blind retry.
  Live on both servers (`tests/common/imip.rs`).
- **Optimistic concurrency rides on the `ETag`, and CalDAV can actually promise it.** It
  advertises `Capabilities::calendar_writes(WriteGuard::Enforced)` — the *other* calendar
  transport cannot (`jmap.md`), which is why the guard is a capability a host reads rather
  than an assumption it makes. A create sends `If-None-Match: *` (never overwrite an
  existing resource at the href); a patch, a document replace, or a guarded delete sends
  `If-Match: "<etag>"` (apply only while the server copy is unchanged), taken from
  `base.revisions` — **the event as the caller read it**, so a guard cannot be
  hand-assembled stale. A failed precondition is `412` → `FailureClass::Conflict`, recovered
  by refetch and re-apply, **never a blind retry** (`error.rs`). `PUT` and `DELETE` are
  **idempotent HTTP methods** (RFC 7231 §4.2.2), and the precondition makes a retry
  self-correcting: a retried create `412`s if the first landed, a retried patch `412`s once
  the ETag moved, and a retried delete sees the resource already gone. So a lost-response
  retry is **safe** — there is no ambiguous `NeedsConfirmation` case as there is for SMTP.
- **`DELETE` is idempotent: already-gone is success.** A `DELETE` whose resource is
  **already absent** (`404`/`410`) resolves as `Ok` (RFC 7231 §4.3.5), not a `Permanent`
  error — so re-running a delete whose response was lost (the first one landed) succeeds
  rather than reporting a spurious failure. A `412` (the resource still exists but its ETag
  moved) remains a genuine `If-Match` conflict, surfaced for refetch.
- **A patch rewrites the stored `RawIcal`, never a re-serialized projection**
  (`calendar-semantics.md`, `modeling.md`): so properties the lossy JSCalendar projection
  cannot express (`X-` props, `VALARM`, the embedded `VTIMEZONE`, …) survive the round trip,
  locked offline *and* proven against both servers. There are therefore **two serializers,
  and they are not interchangeable**:
  - **Create** → `build_event_ical`, a **minimal** RFC 5545 builder (`UID`, `DTSTAMP`,
    `DTSTART`/`DTEND` **in the draft's own form** — zoned, floating or all-day, never
    flattened to UTC — `SUMMARY`, optional `DESCRIPTION`, optional `LOCATION`; TEXT escaped
    per §3.3.11), locked by a round-trip test through the parser (which asserts the `LOCATION`
    lands back in the projection's `locations`, the same field the read path fills). A create
    is the one write that sets a location from nothing; an edit reshapes it through the
    patcher's `LOCATION` path below. It emits at most **seven properties**. Using it to
    *update* an existing event would be data loss: every property it does not emit — the
    `RRULE`, the attendees, the alarms — would be deleted from the user's calendar by a
    `PUT` that reports success. Nothing can: `patch_event` is the only update path, and it
    refuses outright if the base carries no stored `raw_ical` to patch.
  - **Patch** → `patch_event_ical`, the **structural patcher** (`ical::patch`). It takes the
    stored `RawIcal` and the neutral `EventPatch`
    (`SUMMARY`/`DESCRIPTION`/`LOCATION`/`DTSTART`/`DTEND`) and rewrites **only** the content
    lines that changed, plus the `DTSTAMP`/`LAST-MODIFIED`/`SEQUENCE` bookkeeping RFC 5545
    requires of a revision. Every other byte — the original line folding, the document's line
    terminators, properties this crate has never heard of — is preserved verbatim, asserted
    structurally rather than by eyeball (`patch_tests.rs`: strike the patched properties from
    both documents and the remainder must be byte-equal).
  - This machinery is **CalDAV's alone**, and that is the point: a JMAP `update` is already a
    JSON-pointer patch the *server* merges, so it has no use for line folding,
    `DTEND`-vs-`DURATION` exclusion or `SEQUENCE` bookkeeping. Hoisting the patcher would
    have dragged all of it into a crate whose other implementer needs none of it. Only the
    **intent** is neutral.
  - The shared fold-aware line surgery is `ical::lines::Document`, and the shared TEXT
    escaping / date-time rendering is `ical::format`. Both serializers and the `imip` RSVP
    primitive go through them, so there is one implementation of "rewrite this content line,
    leave every other byte alone", not three.

  Three rules the patcher enforces, each of which is a silent-corruption bug if left to the
  caller:
  - **A move may not change a value's *form*.** A new `DTSTART`/`DTEND` must be zoned in the
    same zone / floating / all-day as the one it replaces, or it is an `Err` — never a
    conversion. Rendering an `Europe/Amsterdam` event as UTC moves it for every other reader;
    rendering an all-day event as timed turns a day into an instant. The predicate itself
    (`CalendarDateTime::has_same_form`) lives in `engine-core`, because JMAP needs the
    identical rule and two copies would drift.
  - **`RECURRENCE-ID` targeting is explicit** (`PatchTarget::Series` vs
    `PatchTarget::Instance`), with no default — see `calendar-semantics.md`. Splitting a
    *new* override out of the master is **CalDAV's chore** (copy the master's bytes, drop its
    series rules, splice a `RECURRENCE-ID`), which is why it needs the occurrence's own start
    and end: the master's are the *first* occurrence's. A JMAP server materializes the
    override itself and needs neither — so the neutral `PatchTarget::Instance` must not
    promise them.
  - **An event may not end before it begins.** The check is against the end the event *will
    have*, so moving the start past an unchanged end is caught too; the reader would
    otherwise reject the event as malformed and drop it, making the edit look saved while
    the event vanished.

  `CalDavProvider::event_href` mints the conventional `<collection>/<uid>.ics` resource href
  for a create (percent-encoding the `UID` as one path segment) from the draft's `UID`; a
  patch/delete reuses the stored `EventId`. A draft naming a calendar other than the bound
  collection is **refused**, not silently written to the bound one — `rebind` first.
- **The new `ETag` is read back where the server supplies it.** A successful PUT returns the
  resource's new entity tag in the `ETag` response header (RFC 4791 §5.3.4), surfaced on the
  receipt as `revisions`; when the server omits it the receipt carries an empty
  `RevisionTokens` and the next `sync-collection` delta refreshes `event.revisions`. No
  automatic follow-up `GET` is issued. Both harness servers **do** supply it, and the live
  suite drives the create → patch → delete chain with the delete guarded by the **receipt's**
  `ETag`, never a refetched one — so "write, keep the receipt, write again" is a proven path,
  not an assumption. The *store's* copy is refreshed separately, by the post-write reconcile
  below (issue #65); the receipt chain is what a caller of the **low-level drivers** uses,
  since those do not reconcile.
- **Writes are outbox-mediated** (`store-and-sync.md` Write Contract). The thin drivers
  `engine_sync::create_calendar_event`/`patch_calendar_event`/`delete_calendar_event` (plus
  `put_calendar_document` for the RSVP path) mirror `submit_mail`: a durable `PendingOp` is
  recorded **before** the side effect, claimed under a fenced `OpLease`, and resolved
  `Succeeded`/`Failed` under that lease. The op is serialized on the event's **`UID`** — the
  cross-system identity, which exists before a create has an id and survives a transport that
  assigns its own — so writes to one event never race on either provider. The payload is the
  **intent** (the `EventEdit`), not the document it produced: a conflict recovery re-applies
  it to a *freshly fetched* base, where re-sending the rendered bytes would revert somebody
  else's edit with a write the server happily accepts. The **idempotency key is a
  caller-supplied argument**, not derived from the event: the store dedups enqueue by
  `(account, idempotency_key)` across *every* op state (including terminal), so an
  event-only key would wrongly collapse two distinct edits of one resource into one op.
- **Exposed on the host facade, and the facade reconciles.** `Engine::create_calendar_event`
  / `Engine::patch_calendar_event` / `Engine::delete_calendar_event` (`engine-api.md`) wrap
  these drivers, mirroring `Engine::submit_mail`/`Engine::edit_mail`. A host states the event
  or the edit and drives the write through the facade alone (the neutral write types are
  re-exported from `engine-api`); nothing CalDAV-shaped reaches it. A `412` precondition
  failure surfaces as a `Conflict`.
- **A write refreshes the store through the delta, not through its own bytes** (issue #65).
  The drivers leave the store holding the pre-write document and `ETag` — a `PUT` returns no
  body — so each **facade** write follows it with `engine_sync::reconcile_calendar_events`: a
  `sync-collection` REPORT from the held token, which re-delivers the resource *we* just
  wrote with the server's `calendar-data` inline and its new `ETag`, reports a deleted
  resource as removed (404 → tombstone), and advances the sync-token so the change is not
  re-delivered. One round trip, the read primitive, no new verb.
  - Storing our own bytes optimistically would be wrong twice over: Stalwart **reserializes**
    (see above), so the store's `RawIcal` would not be the server's — and it would **mask a
    server that silently dropped a property**, exactly what
    `patched_update_preserves_the_document` exists to catch. Nor can the `ETag` move without
    the body: the row would then claim a revision whose bytes we do not hold, letting a host
    patch a *stale* body under a *valid* guard and silently revert its own edit.
  - The reconcile re-expands over the window the **store** holds (`store-and-sync.md`), so
    the facade's write methods take no horizon or zone — a write never has to be told what
    the UI is showing, and cannot narrow what the host has expanded.
  - `common::reconcile::read_your_writes` pins the whole chain live — sync → write →
    reconcile → **re-read from the store** → write again — on **both** servers, so the claim
    holds for a reserializing server and a byte-verbatim one alike. Its last leg is the
    point: the second edit's `If-Match` comes from the store, and without the reconcile the
    server refuses it with the `412` this issue is named for.

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
  back to the uppercase iCalendar `PARTSTAT` token. The result is a finished **document**,
  not a property patch, so it feeds `EventWrite::replacing` (guarded by the revision the
  event was read at) through the `engine_sync::put_calendar_document` outbox driver — **no
  new write verb or outbox op**. This is the *only* caller of the whole-document verb, and
  the reason it exists beside the neutral patch spine at all; a transport whose update verb
  is already a patch (JMAP) has no such verb, which is why JMAP RSVP stays deferred until
  the neutral patch can carry a participation status (`jmap.md`). On a CalDAV auto-schedule server (RFC 6638) the changed `PARTSTAT` is what
  the server turns into the iTIP `REPLY` to the organizer. On a server that does **not**
  auto-schedule (`Capabilities::calendar_scheduling` is `false` — the SabreDAV fixture, and
  any plain RFC 4791 server), the same `PUT` stores the answer and tells nobody, and the
  caller has to send the iTIP `REPLY` itself as an iMIP message. The engine now carries
  that: a `Draft` with a `DraftCalendar` (`engine-rfc5322`, `imap-smtp.md`). It does **not**
  build the `REPLY` object — the caller does, because it owns the `UID`/`SEQUENCE` the
  answer keys to.

### What auto-scheduling actually does (observed against Stalwart)

Locked by `provider-caldav/tests/scheduling/mod.rs`, a two-party live exchange between the
harness's scratch accounts. These are server behaviours, not spec readings — several are not
what a client would assume:

- **One organizer `PUT` produces three deliveries.** The server deposits a `METHOD:REQUEST`
  in the attendee's scheduling inbox, **adds the event to the attendee's own calendar**
  already carrying `PARTSTAT=NEEDS-ACTION`, *and* mails the attendee an iMIP invitation. The
  attendee's client wrote nothing and parsed no iMIP.
- **The attendee's copy lives at a server-minted href** (e.g. `1785405220_87408….ics`), never
  at `<uid>.ics`. Anything that addresses a delivered invitation by minting a href from the
  `UID` will miss it; read it back by `UID` through a sync.
- **The RSVP round trip needs no delivery step.** `set_my_partstat` + a guarded `PUT` of the
  attendee's copy is enough: the organizer's *separate* resource comes back
  `PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0`. Stalwart applies the reply as a **targeted
  patch** — the organizer's own `PRODID` survives — so it does to the organizer's document
  what our patcher does to ours.
- **The organizer's `DELETE` does not remove the attendee's copy.** It arrives as an iTIP
  `CANCEL` and the attendee's resource is rewritten with `STATUS:CANCELLED` (a tombstone in
  the projection). A host listening only for deletions would leave a cancelled meeting
  looking live; a test that cleans up must delete *both* copies.
- **The server bumps `SEQUENCE`** (`0` → `1`) when it processes the organizer's create, and
  **re-quotes a `TZID` containing spaces** (`TZID="W. Europe Standard Time"`, RFC 5545 §3.1).
  Both are why the assertions are on parsed content, never on bytes.
- **Delivery is asynchronous.** The `PUT` is answered before the other party's copy is
  updated, so every cross-account read is a bounded poll on real state, never a sleep.
- **Scheduling cannot run on the seeded account.** The iMIP mail above reaches the attendee
  *and* the organizer (a `REPLY` arrives as "Accepted:…"), and the mail suites assert an exact
  INBOX count on Alice — so the whole exchange runs between two scratch accounts
  (`stalwart-harness.md`). This is why `bob`/`carol` exist.
- **That mail is rate-limited, and exceeding the limit looks like nothing at all.** Stalwart's
  default inbound throttle is 25 messages/hour per (sender domain, recipient); past it the
  server abandons the **whole** iTIP delivery — calendar copy included — while still answering
  the organizer's `PUT` with `201`, and logs nothing. The harness raises the throttle at
  bootstrap so the suite is re-runnable (`stalwart-harness.md`).

## Known limitations (documented, not bugs)

- **iTIP/iMIP inbound parse + RSVP are implemented; delivery/persistence wiring is
  staged.** `engine_core::scheduling` (keys, `SEQUENCE` ordering, the trust
  decision, `reconcile` → `ScheduleAction`, the `apply_reply`/`cancel` event
  mutations) and `provider_caldav::imip` (parse + `set_my_partstat`) are done and
  offline-tested end to end through the conditional-`PUT` outbox driver, and the
  RSVP round trip is **live-proven against Stalwart's auto-scheduler** (above).
  Still deferred (`calendar-semantics.md`): the **CalDAV Scheduling Inbox**
  `REPORT` (RFC 6638) — the live suite reads the inbox over raw DAV, precisely
  because the provider does not expose it yet; **driving `reconcile`/apply from a
  real sync** (the part *fetch* landed as `Engine::message_scheduling`); and
  **`ClientImip` local-origin persistence** (storing a brand-new inbound
  `REQUEST` has no provider-less single-event store path yet).
  **Client-iMIP `REPLY` delivery over SMTP is no longer deferred** (issue #105): the
  assembler carries a `text/calendar` alternative body part with its `method=` parameter,
  and `Capabilities::scheduling_submission` says which transports can send one. What is
  still the caller's job is *building* the `REPLY` object — the engine has no
  `Event` → iTIP serializer, and the answer keys to a `UID`/`SEQUENCE` only the caller
  holds.
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
- **CardDAV collection mutation is out of scope.** Existing address books and
  group cards are readable; address-book/group create, rename, delete, and
  membership mutation remain deferred.
- **Custom (non-IANA) `VTIMEZONE` expansion is staged.** A `TZID` is resolved as
  an IANA zone; a genuinely custom embedded `VTIMEZONE` is preserved in `RawIcal`
  but not parsed into the expander, so such an event stores with no occurrences
  (the staged behavior `calendar-semantics.md` describes for embedded zones).
  Recording which source was used, and the disagrees-with-IANA fixture, ride with
  that slice.
- **`RRULE UNTIL` with a `Z` (UTC) bound** is read as its wall-clock value;
  converting it to the event's zone needs tzdata and is staged. The supported seed
  uses `COUNT`, not `UNTIL`.
- **Calendar CalDAV has no CTag fallback yet.** Calendar event sync still relies
  on RFC 6578. CardDAV implements CTag + per-resource ETag snapshot fallback
  when `sync-collection` is unavailable.
- **Calendar events are fetched whole, not paged** — consistent with the JMAP
  calendar slice (events have no natural recency sort, and the REPORT returns the
  collection in one pass).

## Testing

- **Offline (always green, no Docker):** the iCalendar parser, the WebDAV
  multistatus parser, the normalizers, and the cursor/snapshot/delta logic are
  unit-tested, including an adversarial panic-resistance pass over hostile
  iCalendar (the `fuzz/` `caldav_parse` cargo-fuzz counterpart). Captured,
  secret-free transcripts (`tests/fixtures/`) — Stalwart's, plus SabreDAV's calendar
  home for the read-only privilege case — drive the `dav`/`discovery`/
  `calendar`/`sync` layers through a **fake `DavExecutor`**, including the
  `current-user-privilege-set` → `CalendarAccess` mapping and its
  reported-nothing / reported-empty / `404`-propstat edge cases. A **full offline sync
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
  → `EventWrite::replacing` → `engine_sync::put_calendar_document` into a real
  `SqliteStore` over the fake executor (asserting the `If-Match` `PUT` carries my
  accepted `PARTSTAT` and no transit-only `METHOD`), plus the security case that a
  parsed `REQUEST` whose `ORGANIZER` mismatches the authenticated sender is rejected,
  not written.
- **Live, against two real servers.** `tests/live_caldav.rs` (gated on
  `STALWART_HTTP_ADDR`, run by the `stalwart` CI job) and `tests/live_sabredav.rs`
  (gated on `SABREDAV_HTTP_ADDR`, run by the `sabredav` job) both connect over HTTP, run
  discovery + `sync-collection`, and assert the seed invariants in the store plus an
  idempotent **empty delta** on a second sync (the held sync-token). Both then run the
  **same four write scenarios** from `tests/common/write.rs`. Every test in a binary
  **serializes** on a shared guard — each scenario transiently adds an event, which must
  not race the exact-count assertion or another scenario — and each pre-cleans its own
  residue, so an interrupted run never wedges the next one. Both files are excluded from
  the offline coverage metric, like the JMAP/IMAP live tests.

  Writes are where a live server is not optional: the offline `Replay` executor serves
  canned bytes **without reading the request**, so it can confirm how we handle a
  response and *nothing* about whether the server accepted what we sent. The four
  scenarios are exactly the questions it cannot answer:

  | Scenario | What only a real server can tell you |
  | --- | --- |
  | `round_trip` | The `PUT` returns the new `ETag`, and it is the resource's real one — the whole create → update → delete chain runs off the receipts, never a refetch. |
  | `patched_update_preserves_the_document` | An edit made with the **structural patcher** survives the *server*. Our byte-equality tests prove the patcher keeps the `RRULE`, `VALARM`, `VTIMEZONE` and `X-` properties; they say nothing about whether the server stores them. |
  | `stale_if_match_is_a_conflict` | A superseded `If-Match` really returns `412` (on `PUT` **and** `DELETE`), and the adapter classes it `Conflict` — refetch-and-merge — not blind-retryable. |
  | `instance_override_split_is_accepted` | A `RECURRENCE-ID` override the patcher splits out of a master is accepted as part of the same resource, and comes back folded into one event. |

  **Scheduling (Stalwart only): `tests/scheduling/mod.rs`.** Five more scenarios, declared
  only by `live_caldav.rs` because they need an auto-schedule server and two principals —
  SabreDAV has neither. They drive a real invitation between the two scratch accounts:
  delivery to the attendee's calendar, the quoted Windows `TZID` resolving to `Europe/Berlin`,
  the scheduling-inbox `METHOD:REQUEST` parsing, the **RSVP arriving on the organizer's own
  copy**, and an organizer cancel tombstoning the attendee's copy. What only a real server can
  tell you here is the reply: it is a thing the *server* does to a *second account's* resource
  in response to bytes we sent, so no fake can stand in for it. Cross-account reads are bounded
  **polls on real state** (iTIP delivery is asynchronous), never sleeps.

  | Scenario | What only a real server can tell you |
  | --- | --- |
  | `an_invitation_is_delivered_to_the_attendee` | The server puts the event on the *attendee's* calendar, at a href it mints itself, with `PARTSTAT=NEEDS-ACTION` — no client-side iMIP anywhere. |
  | `an_invitations_windows_time_zone_resolves_to_iana` | Stalwart accepts a Windows `TZID` with no `VTIMEZONE` and hands it back **DQUOTE-quoted**; the parser must strip *and* CLDR-map it. |
  | `the_scheduling_inbox_carries_a_parseable_itip_request` | A transit-form iTIP document written by someone other than our own serializer parses. |
  | `an_rsvp_reaches_the_organizer` | Patching `PARTSTAT` and `PUT`ting it back is the whole RSVP: the organizer's separate copy reads `ACCEPTED`. |
  | `an_organizer_cancel_marks_the_attendees_copy_cancelled` | A cancel *tombstones* the attendee's resource rather than removing it. |

  Two things to know before editing this suite. It runs entirely **off the seeded account** —
  auto-scheduling mails both parties and Alice's INBOX count is asserted elsewhere. And it only
  works because the harness **disarms Stalwart's inbound rate limiter** at bootstrap (25
  messages/hour per sender-domain→recipient by default); past that cap the server silently
  abandons the whole iTIP delivery and every scenario here times out. `stalwart-harness.md`
  has the details.

### Which server proves what

The two fixtures are **not interchangeable as evidence**, and the difference decides how
the preservation assertion is written. Observed against both:

| | **Stalwart** | **SabreDAV** |
| --- | --- | --- |
| Stored iCalendar | **Reserializes** — keeps every property, but re-folds content lines and reorders `RRULE` parts | **Verbatim** — the bytes you `PUT` are the bytes you `GET` |
| `ETag` on `PUT` | Yes (create + update) | Yes (create + update) |
| Stale `If-Match` | `412` | `412` |
| Master + `RECURRENCE-ID` in one resource | Accepted | Accepted |
| Read-only collection | **Cannot produce one** — the harness account owns everything it can see | **The privilege fixture**: Bob's calendar, shared to Alice read-only |
| Scheduling (RFC 6638) | **Full auto-schedule** — advertises `calendar-auto-schedule`, exposes `CALDAV:schedule-inbox-URL` | **None** — the fixture loads no `Schedule\Plugin`, so its `DAV:` header lists `calendar-access` and not `calendar-auto-schedule` |
| `Capabilities::calendar_scheduling` | `true` | `false` — **the only negative case in the repo** |

Two consequences to keep in mind before touching these tests:

- **A preservation assertion may never compare our bytes with the server's** — Stalwart's
  reserialization would fail it for a formatting difference that lost nothing. It
  compares the **server's own copy before the patch** with the **server's own copy after
  it**, on unfolded logical lines, with the properties the patch may touch
  (`SUMMARY`/`DTSTAMP`/`LAST-MODIFIED`/`SEQUENCE`) struck out. Whatever the server does
  to formatting it does to both copies; anything it *drops* shows up as a missing line.
  The check is verified to bite: replacing `patch_event_ical` with the projection-
  rebuilding `build_event_ical` fails it.
- **`DAV:current-user-privilege-set` is locked by SabreDAV, not Stalwart.** Its seed
  (`docker/sabredav/entrypoint.sh`) gives Alice a second collection — one Bob owns and
  shares with her **read-only** — so one `PROPFIND` of one calendar home returns two
  collections with two different answers, and `sabredav_reports_a_read_only_share_as_unwritable`
  asserts both. Stalwart's account owns every calendar it can see, so it can only prove
  the writable half (`caldav_reports_the_bound_calendar_as_writable`). The offline locks
  are captured transcripts of both servers' real responses
  (`tests/fixtures/calendar-home.xml`, `calendar-home-sabredav.xml`).
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
