# Calendar Semantics

This document fixes three calendar concerns the high-level docs leave open: time
and timezone handling, inbound scheduling (iTIP/iMIP), and the
JSCalendar↔iCalendar normalization boundary. It complements the recurrence
materialization in `store-and-sync.md` and the calendar invariants in
`north-star.md`. Read it before working on calendar normalization, recurrence
expansion, or scheduling.

## Time and timezones

- **IANA tzdata is the single source of truth, bundled and version-pinned** —
  not the host OS database. A user's devices must expand recurrence identically,
  so determinism beats matching the local OS. The bundled tzdata version is
  recorded. Expansion lives in the `engine-recurrence` crate and resolves zones
  through `jiff` + `jiff-tzdb`, pinned with `default-features = false` +
  `tzdb-bundle-always` so jiff never reads `/usr/share/zoneinfo`, `TZDIR`, or the
  system zone (the bundle-only mode jiff's own docs prescribe — its system source
  otherwise takes precedence). The recorded version is `jiff_tzdb::VERSION`.
- Each materialized occurrence records the tzdata version it was expanded under.
  A version bump invalidates and re-expands affected occurrences through the
  store maintenance path (`store-and-sync.md`); occurrences whose zones did not
  change stay byte-stable.
- **Embedded `VTIMEZONE` reconciliation.** iCalendar may carry custom timezone
  definitions that disagree with IANA:
  - If the `TZID` resolves to a known IANA zone, expand with IANA (consistent and
    updatable) and preserve the embedded `VTIMEZONE` in `RawIcal`.
  - If the `TZID` is unknown or custom, expand using the embedded `VTIMEZONE`
    rules.
  - Record which source was used. A `VTIMEZONE`-disagrees-with-IANA fixture is
    required.
- **Floating time** (no zone) is wall-clock on the master event, resolved to an
  instant in the observer's (host) zone. Because `event_occurrence` rows are UTC
  instants, the expander resolves a floating series through the host zone supplied
  at materialization; a host-zone change re-expands the floating events through the
  maintenance path (the same mechanism as a tzdata bump). A floating event's
  membership in a time range can therefore shift with the host zone — that is
  inherent to floating time, not a defect.
- **All-day / date-only** values are zoneless calendar dates: no DST, never
  attach a zone.
- **Read-side display resolution.** A host rendering a single stored event's
  start (not the materialized occurrence rows) resolves it through
  `engine_recurrence::resolve_instant` (re-exported from `engine-api`), the
  read-side counterpart to expansion: a zoned start — UTC (`Etc/UTC`) or a named
  IANA zone — resolves to its absolute UTC instant through the *same* bundled
  tzdata, so the host can localize to the device zone regardless of the authoring
  zone; a floating or all-day value returns `None` (no fixed instant) and the host
  shows wall-clock/date text; a custom/embedded zone returns `UnsupportedZone`.
  Hosts must localize off the resolved instant — never the bare wall-clock — or a
  non-UTC event displays in the wrong zone.
- **The UTC→local direction, for hosts that lay out geometry.** Expansion and
  `resolve_instant` both go local→UTC. A host that *renders a grid* needs the inverse:
  which local day column an occurrence falls in, and which row. `engine_recurrence::to_local`
  (re-exported from `engine-api`) gives the wall clock an instant shows as in a zone, and
  `day_bounds_utc` gives the UTC window a local calendar day occupies — the window to
  range-read occurrences for.

  Both exist so a host never bundles a **second** tz database to do this itself. Two
  tzdbs mean two answers; `event_occurrence` already records the release it was expanded
  under (`tzdata_version`) precisely because that divergence matters.

  Position events by the returned **wall clock**, not by elapsed minutes from local
  midnight, and take a day's length from `day_bounds_utc` rather than adding 24h. A day
  is not always 24 hours: on the spring-forward day it is 23 and on the fall-back day it
  is 25. Adding an absolute 24h to local midnight clips an hour of events off one and
  pulls the next day's first hour into the other.
- **Range reads go through the occurrence rows, never the events.** `StoreRead::scope_occurrences`
  (facade: `Engine::occurrences_in`) is the read a calendar grid pages over. `Engine::events`
  returns the projected *envelope* — a recurring series is **one** object, at its series
  start — so a host that lays out `events()` renders a weekly meeting in exactly one week
  and no other. The window is half-open at both ends, so an event ending exactly when a
  week opens belongs to the previous page only and paging forward never double-renders it;
  an event that merely *spans* the window is still returned, because it has to render on
  every day it covers.
- **Total-order key for sorting + the display zone.** Sorting an agenda that mixes
  zoned, floating, and all-day values needs an instant for *every* value, so
  `resolve_instant_in(value, host_zone)` resolves a floating value through the
  caller's display zone and an all-day value to that zone's local midnight (a
  zoned value still resolves through its own zone). The display zone is a **host
  app preference**, not engine-domain state: the host detects the OS zone natively,
  the product-core persists the user's chosen zone, and the engine only resolves
  and validates against it. `is_supported_zone` lets a host reject a picked or
  device-reported zone the bundled tzdb cannot resolve before adopting it.
- Normalization target: JSCalendar (`LocalDateTime` + IANA `timeZone`, or UTC)
  and iCalendar (`DTSTART` with `TZID`/`VTIMEZONE`, UTC `Z`, or floating) both map
  to one engine time model — an instant resolved through its zone, or wall-clock
  for floating.
- **Adapters may deliver non-IANA zones.** Microsoft Graph uses Windows zone
  names (and `tzone://Microsoft/Custom` for legacy custom zones). The adapter
  maps these to IANA at its boundary (CLDR `windowsZones`); the engine time model
  is IANA-only. **Google Calendar is IANA-native by contrast** (`provider-google`):
  event times carry an IANA `timeZone` directly, so no zone table is needed — the
  adapter pairs the wall clock (the RFC 3339 `dateTime` stripped of its offset) with
  that zone.
- **Out of scope:** `RSCALE` / non-Gregorian recurrence (RFC 7529) is preserved
  raw, not expanded.

## Inbound scheduling (iTIP/iMIP)

The Write Contract covers *outbound* scheduling. Inbound is the missing half:
recognizing and reconciling scheduling messages that arrive through sync. The
**inbound parse/reconcile/trust/apply pipeline and the RSVP write primitive are
implemented**; the precise deferrals are listed at the end of this section.

- **iMIP is iTIP over email:** a message with a `text/calendar` part carrying a
  `METHOD`. The mail sync path must detect these and hand them to the calendar
  layer — this is the mail↔calendar bridge. **Implemented:** the detection step is
  `engine_core::scheduling::find_calendar_part` (a pure walk of the MIME tree for a
  `text/calendar` part), and the parse is `provider_caldav::imip::parse` →
  `engine_core::scheduling::SchedulingMessage` (the iCalendar parser, reused, plus
  the `VCALENDAR` `METHOD` and the `VEVENT` `DTSTAMP`). *Fetching* the part's bytes
  and parsing them is `Engine::message_scheduling` (see "Reading an invitation out
  of mail" below) — cache-first on the raw source, so a message whose body was
  already read costs no extra provider fetch. What remains deferred is driving
  `reconcile`/apply from a sync pass, not reaching the bytes.
- **The parser is transport-neutral, deliberately.** iCalendar is not a CalDAV
  format: iMIP carries it over *mail*, on every account type (RFC 6047), so the
  parser lives in `engine-ical` beside the model it produces, and CalDAV is one of
  its callers. A Gmail- or Graph-only build must still be able to read an
  invitation.
- **Capability split.** Prefer server-side scheduling where the provider has it:
  CalDAV Scheduling Inbox (RFC 6638) or JMAP Calendars scheduling. Pure
  IMAP/SMTP has none, so the client parses iMIP from the mail stream and sends
  iMIP replies. Adapters expose which model applies; callers do not switch on
  provider kind.
- **Identity.** The invite email stays a normal mail provider object with its raw
  preserved; the derived event is a separate projection. Do not conflate their
  identities. Reconcile scheduling by `(UID, SEQUENCE, RECURRENCE-ID)`, never by
  email identity — the same `UID` can arrive repeatedly and across folders. A
  higher `SEQUENCE` supersedes; `RECURRENCE-ID` targets a single instance.
  **Implemented:** `SchedulingMessage::instance_key()` keys on `(UID,
  RECURRENCE-ID)` and `::revision()` on `(SEQUENCE, DTSTAMP)`; `reconcile`'s
  supersession gate drops a message that does not strictly supersede the highest
  revision already applied for its key (the synthetic `EventId`/`CalendarId` a
  parsed message carries are placeholders — storage identity is assigned later).
- **`METHOD` handling.** **Implemented** as `engine_core::scheduling::reconcile`
  returning a `ScheduleAction` (after the trust gate and supersession check):
  - `REQUEST` → `ScheduleEvent` (create or update; attendees default to
    needs-action).
  - `REPLY` → `RecordReply { attendee, status }`, applied to the organizer's stored
    copy by `apply_reply`.
  - `CANCEL` → `Cancel`, applied by `cancel` (a series cancel tombstones the event;
    an instance cancel excludes that occurrence).
  - `COUNTER` / `DECLINECOUNTER` / `REFRESH` / `ADD` / `PUBLISH` → `Surface(method)`
    — classified and surfaced to the host; full handling stays staged.
- **Responding is a neutral verb of its own**, not an edit of the attendee array —
  `Engine::rsvp_calendar_event(provider, account, idempotency, &base, &EventRsvp)`, outbox-mediated
  and reconciling like every other calendar write. It is a separate verb because it does something no
  edit does: it makes the **server tell the organizer**. Patching `participants` would change the
  same bytes and skip the scheduling entirely, on every transport.

  `EventRsvp` carries the answer (`RsvpResponse` — a closed `Accepted`/`Tentative`/`Declined`, so
  "RSVP needs-action" is unrepresentable), the **matched** attendee address (an alias invitation
  answers as the alias — never the account's primary identity), an optional `comment`, and
  `notify_organizer`. Four adapters render it:

  | Provider | How |
  |---|---|
  | CalDAV | `imip::set_my_partstat` rewrites *my* `PARTSTAT` in the stored raw (every other property survives verbatim), then a conditional `PUT`. An RFC 6638 auto-schedule server emits the `REPLY` itself. |
  | Graph | `POST /events/{id}/accept\|tentativelyAccept\|decline` with `comment` + `sendResponse` |
  | Google | `events.patch` on the attendee's `responseStatus`, with `sendUpdates=all\|none` |
  | JMAP | `CalendarEvent/set` `update` of `participants/<my id>/participationStatus`, with `sendSchedulingMessages` carrying `notify_organizer` |

  **`Capabilities::calendar_rsvp` is not optional reading.** It is `Option<RsvpControls>`: whether the
  transport can answer at all, whether a `comment` has anywhere to go, whether the user may decline to
  notify, and — separately from `calendar_write_guard` — how strong the guard on *this* request is.
  That last field exists because Graph's action endpoint accepts no `If-Match` while its `PATCH` does;
  reporting one number for the adapter would hide it. An adapter **refuses** a control it cannot
  honour (`RsvpControls::accept`, one implementation shared by all four) rather than dropping it: a
  note that silently goes nowhere, or an "Email organizer" tick that emails them anyway, is worse than
  a control the user was never shown.

  **Whether anyone is told is a second capability, and on CalDAV it is discovered**
  (issue #105). `calendar_rsvp` says the transport can *express* an answer;
  `Capabilities::calendar_scheduling` says the **server** performs the scheduling the write
  implies. RFC 4791 is calendar access and RFC 6638 is a separate layer, so a plain CalDAV
  server stores the rewritten `PARTSTAT` correctly and emits no `REPLY` — the silent success
  `RsvpControls` exists to prevent, arriving from the transport instead of from a caller.
  `provider-caldav` asks at connect (`OPTIONS` → the `calendar-auto-schedule` token in the
  `DAV:` header); Graph, Google and JMAP are constants (`providers.md`).

  **Carrying a client-side `REPLY` is now implemented.** When `calendar_scheduling` is
  `false` the caller must send the iTIP object itself, and a `Draft` carrying a
  `DraftCalendar { ical, method }` assembles it as a conformant `text/calendar` alternative
  body part (RFC 6047 §2.4 — `imap-smtp.md`). `Capabilities::scheduling_submission` says
  which transports can: IMAP/SMTP, Graph and Google yes; **JMAP no, and it refuses the
  draft** rather than sending an unprocessable one (`jmap.md`). Read the two capabilities
  together — between them they answer whether a `ClientImip` account can answer *at all*.
  What the engine still does not do is **build** the `REPLY` object: there is no
  `Event` → iTIP serializer, and the answer keys to a `UID`/`SEQUENCE` the caller holds.

  **And "the server schedules" is still not "the organizer was told".** A capability is
  discovered once, at connect, and cannot say *…and it works*. Only the server knows that,
  after the fact, per answer — so an RSVP receipt carries
  `EventWriteReceipt::reply_delivery`, a neutral `ReplyDelivery` of `Delivered` / `Failed` /
  `NotReported` / `Unrecognized`. Two rules bind every caller:

  - **`NotReported` is not success.** Most transports never report, and one real CalDAV
    deployment reports a permanent failure on *every* reply while advertising
    auto-scheduling. Treating silence as delivery renders that failure to the user as "You
    accepted"; treating it as failure cries wolf on everyone else. Branch on
    `ReplyDelivery::failed()`, which is the only actionable state.
  - **Only CalDAV fills it in**, because only CalDAV has somewhere to put the answer (RFC
    6638 §3.2.9 `SCHEDULE-STATUS`, read back off the stored object). Graph returns `202
    Accepted` with no body and Google's `sendUpdates` is fire-and-forget — both confirm the
    *write*, never the notification — and JMAP cannot send iMIP at all. That is a genuine
    protocol difference, not an adapter gap: see `providers.md`. Those three own delivery
    end to end and surface their own failures to the user's mailbox, so there is nothing a
    host could usefully do with a verdict they cannot give.

  **The answer has to read back, too**, and that is a normalization rule, not a write
  concern: a projection holds **one participant per address**, with roles as a *set*
  (JSCalendar's model — `engine-ical`'s `party` module states it for iCalendar's separate
  `ORGANIZER`/`ATTENDEE` properties, and `provider-google`'s `participants` for Google's
  `organizer` object beside its `attendees[]` entry). Emitting a synthesized organizer
  *beside* the attendee entry for the same person publishes two contradictory statuses for
  one address, and since only the attendee entry is what an RSVP writes, a host that looks
  its own address up can read an `accepted` it never gave. Google shipped exactly that bug
  and it presented as a broken *write* — the status looked frozen while every patch had in
  fact landed.

  **Which status survives the merge is a per-provider fact, and the two are opposite.**
  Google tracks the organizer's own `responseStatus` (it starts even a self-organized event's
  entry at `needsAction` and moves it on `events.patch`), so the attendee entry is
  authoritative there. Graph never records a response *from* an organizer and writes
  `"none"` in that slot, so adopting it would report the person who called the meeting as
  not having answered — the owner's implied acceptance stands instead, while `"none"` on a
  real guest still means `needs-action`. Both are proven by live tests against invitations
  the account did not organize; neither is a reading of a spec. Whether the organizer even
  appears in the attendee list also depends on **whose copy** it is: Graph omits them from
  the organizer's own copy and includes them in an invitee's.

  The guard on an RSVP is `EventRsvp::guard` — the revision the *caller* read, recorded in
  the outbox when the user answered — never the base event's current revision at drain
  time, and `None` means "answer unconditionally". CalDAV and Google both read the intent's
  guard; Graph cannot send one at all (its action endpoint takes no `If-Match`, so it
  advertises a weaker `RsvpControls::guard`).
- **Security.** Scheduling messages are hostile input. Validate `ORGANIZER` and
  attendee identities against the message's authenticated sender (From / DKIM /
  authenticated submission) before applying anything; never auto-apply changes
  from an unauthenticated or mismatched sender. **Implemented** as the trust gate
  that runs **first** in `reconcile`: `SchedulingMessage::trust` →
  `evaluate_imip_trust` rejects an unauthenticated or identity-mismatched message
  (`ScheduleAction::Rejected`) before its contents are considered.

**Deferred (documented, not bugs):** (1) **driving `reconcile`/apply from a real
sync** — the part *fetch* is done (`Engine::message_scheduling`, below), but no
sync pass feeds it into `reconcile` and applies the result; (2) **`ClientImip`
local-origin persistence** — storing a brand-new inbound `REQUEST` as a local
event has no provider-less single-event store path yet (the store's writes are
sync- or outbox-mediated), so the apply helpers run but persisting a
not-yet-on-a-server event waits on that path; (3) the **CalDAV Scheduling Inbox**
`REPORT` (RFC 6638) — the live suite reads the inbox over raw DAV rather than
through the provider, which is exactly the gap; and (4) an **`Event` → iTIP
serializer**, so a caller composing a client-side `REPLY` still writes the
iCalendar itself. **Carriage** of that reply is no longer deferred (#105): see
`Capabilities::scheduling_submission` above.

The `ServerAutoSchedule` RSVP path (now behind the neutral verb) is fully wired,
offline-tested end to end, **and live-proven**: `provider-caldav`'s scheduling
suite runs a real two-party exchange against Stalwart's auto-scheduler and
asserts that the organizer's own copy comes back accepted, with no client-side
iTIP delivery of any kind. `caldav.md` → "What auto-scheduling actually does"
records the server behaviours that run pinned down, several of which contradict
the obvious assumption (the attendee's copy appears at a server-minted href; an
organizer's `DELETE` tombstones rather than removes; the server also *mails* both
parties).

**Reading an invitation out of mail: what really arrives.** The read is
`Engine::message_scheduling` — it finds the `text/calendar` part in the MIME tree,
decodes it (base64, quoted-printable and declared charsets all occur), parses it
with the one hardened parser, and returns it alongside the addresses the message
was **delivered to** (the MTA `Delivered-To`/`X-Original-To`/`Envelope-To` headers
first, then `To:`/`Cc:`) — which is what makes an invitation to an alias work with
no configuration. A payload that will not parse yields `None`, never an error; the
mail is still readable. Two things a real server-authored invitation taught us
(fixture: `engine-api/tests/fixtures/stalwart-invitation.eml`):

- **A genuine `METHOD:REQUEST` can arrive as a dispositioned attachment.** Stalwart
  sends the calendar part as a top-level sibling in a `multipart/mixed` carrying
  `Content-Disposition: attachment; filename="event.ics"` — not as an
  undispositioned `multipart/alternative` body part. So `from_inline_body` answers
  "was this a body part" and nothing more: the RSVP gate is a scheduling `METHOD`
  **plus** an `ATTENDEE` matching one of the account's own addresses, and a host
  that gated on `from_inline_body` instead would discard every such invitation. The
  corollary is that `event.ics` keeps its attachment chip — the suppression rule
  hides the *undispositioned* iMIP body part (the Gmail/Outlook shape), and must not
  hide a file the sender marked as one.
- **Assume nothing about how tidily the payload is encoded.** That same part is
  quoted-printable, its Windows `TZID` is DQUOTE-quoted *and* QP-escaped, and its
  `ATTENDEE` line is folded mid-`mailto:` (`mailt` + CRLF + ` o:carol@…`). Unfold or
  decode wrongly and the attendee address becomes `mailt`, which matches nobody —
  the invitation silently stops being *mine* and no RSVP is offered.

## JSCalendar ↔ iCalendar boundary

- The normalized projection is JSCalendar-shaped. iCalendar from CalDAV is
  converted into it; JMAP supplies JSCalendar directly.
- The conversion is **lossy**: `VALARM`↔alerts nuance (action, repeat),
  properties with no JSCalendar peer (some `X-` properties and parameters),
  `ATTACH`, certain `ROLE`/`PARTSTAT` edge cases, and some
  `RECURRENCE-ID`/`THISANDFUTURE` semantics.
- Providers also express recurrence structurally rather than as `RRULE` text —
  Microsoft Graph uses a `patternedRecurrence` with series-master / occurrence /
  exception items and a separate cancelled-occurrence list. Normalization maps
  Graph's structured form, Google/iCalendar `RRULE` strings, and JSCalendar
  `recurrenceRules` into one override/exclusion model; round-trips use raw. (For
  Graph this is **implemented for the series master + rule**; its per-instance
  `exception`/cancellation *overrides* are deferred — Graph v1.0 exposes no
  recurrence-id to key them, and the master↔override dedup is itself staged, so a
  Graph `exception` is dropped rather than mapped. See `graph.md` → "Calendar".)
  **Google** is the same posture but *without* Graph's data-loss: it returns masters
  with an `RRULE` (parsed by the shared `engine_core::calendar::parse_rrule`) and
  *does* expose `recurringEventId` on an override — but override reconciliation is
  still staged, so `provider-google` drops a `recurringEventId` instance for now
  rather than mapping it. See `google.md` → "Google Calendar".)
- **The rule renders back out through one shared function too.**
  `engine_core::calendar::format_rrule` is the inverse of `parse_rrule` and the only
  place an `RRULE` value string is built, for every transport that carries one
  (iCalendar/CalDAV, Google's `recurrence` array). It emits a canonical spelling —
  parts in a stable order, RFC 5545 defaults (`INTERVAL=1`, `WKST=MO`) omitted — so
  the same rule always produces the same bytes.
  `parse_rrule(format_rrule(rule, …)?)` is the identity; the reverse is **not**, and
  is not meant to be, because the parser normalizes. Preserving the original bytes
  stays the raw payload's job.

  Two things it will not approximate, because either would store a *different*
  series and report success:
  - A **non-Gregorian** rule (`RSCALE`, RFC 7529) is an error. Dropping the `RSCALE`
    would silently make it Gregorian, against this document's own "preserved raw,
    never expanded" rule.
  - **`UNTIL` is not rendered from the rule alone.** `RecurrenceBound::Until` holds a
    wall clock in the event's zone, while RFC 5545 §3.3.10 requires UTC whenever
    `DTSTART` is zoned or UTC — and resolving a wall clock through a zone needs
    tzdata, which `engine-core` deliberately does not have. So the caller states the
    form through `UntilForm` (`Date` / `Floating` / `Utc(instant)`) and resolves the
    instant itself. A series ending on the wrong day for readers in another zone is
    the bug that shape makes unrepresentable.
- **Rule:** `RawIcal` and `RawJsCalendar` are preserved beside the projection
  (model invariant). Provider writes round-trip from raw plus targeted patches,
  never by re-serializing the lossy projection. The projection exists for
  display, search, and engine logic and is explicitly **not**
  round-trip-authoritative. The CalDAV write slice enforces this — a `PUT` carries
  the round-tripped `RawIcal`, locked by a test that an updated event's `X-`
  property and `VALARM` survive on the wire (`caldav.md`).

## Editing an event: the targeted patch

The targeted patch the rule above demands is **implemented on both calendar transports**,
and they implement it in opposite ways — which is exactly why the engine models the *intent*
and not the surgery. A host states an `EventPatch` (what changed) and a `PatchTarget` (on
which occurrence), both in `engine-provider`, and the adapter does the rest:

- **CalDAV** has no partial write: `PUT` replaces the whole resource, so the **client** must
  do the surgery. `provider_caldav`'s structural patcher takes the stored `RawIcal`, changes
  only the properties the user changed, and leaves every other byte alone — the original
  folding, the line terminators, the properties it has never heard of. All of RFC 5545 line
  folding, `DTEND`-vs-`DURATION` exclusion and `SEQUENCE` bookkeeping lives there.
- **JMAP** `CalendarEvent/set` `update` *is* a patch (a JSON-pointer PatchObject), so the
  **server** does the surgery. There is no JSCalendar serializer in `provider-jmap`, and none
  should be added. Verified live, not assumed: an `update` of `title` alone leaves the zone,
  the duration and the recurrence untouched (`jmap.md`).

Read `caldav.md` → "CalDAV writes" and `jmap.md` → "Calendar writes" for the two renderings;
this section fixes the *semantics* they share.

- **Never rebuild a document to update it.** A create-path serializer emits only the handful
  of properties it knows about. An update that went through one would delete the recurrence
  rule, the attendees, the location, the alarms and the timezone from the user's calendar,
  and report success while doing it. This is the single most destructive thing the calendar
  layer can do, which is why create and update are **different verbs with different inputs**
  — and why the CalDAV patch path refuses outright if it has no stored raw to patch, rather
  than falling back to rebuilding.
- **Which occurrence an edit lands on is the user's decision, not a default.** A drag on
  Tuesday's standup is either a move of *that occurrence* or of *every Monday from now
  to eternity*. `PatchTarget` therefore has no default:
  - `Series` edits the series itself — every occurrence moves. (iCalendar: the master
    `VEVENT`. JSCalendar: the top-level object, leaving `recurrenceOverrides` alone.)
  - `Instance(recurrence_id)` edits one occurrence, named by its **original** start — its
    identity within the series, not where it is being moved to. (iCalendar: the
    `RECURRENCE-ID` override `VEVENT`. JSCalendar: the `recurrenceOverrides` entry keyed by
    that start, RFC 8984 §4.3.3.) The recurrence id must be in the series' own time form: a
    zoned series is not overridden by "the same moment" expressed in UTC — that keys an
    override the series has no instance at, a silent no-op.

  A host that does not ask the user which one it meant will eventually rewrite someone's
  weekly standup for all time.
- **Materializing an override the series does not have yet is *not* uniform, and the neutral
  type must not pretend it is.** CalDAV has to do it by hand: copy the master's `VEVENT`
  (attendees, alarms, `X-` props and all), drop its series-level `RRULE`/`RDATE`/`EXDATE`
  (RFC 5545 §3.8.5 — an override describes one instance), splice in a `RECURRENCE-ID`, and
  land the patch on the copy. That copy inherits the master's `DTSTART`/`DTEND`, which are
  the **first** occurrence's times, so a CalDAV split additionally **requires the
  occurrence's own start *and* end** on the patch — deriving them would mean expanding the
  recurrence rule, which `provider-caldav` does not do (that is `engine-recurrence`). Pass
  them unchanged when the edit is not a move. Left to guess, the override claims the series'
  opening slot: it once produced an override running from 26 Jan 14:00 to 5 Jan 10:00, a
  negative duration the reader then silently discarded as malformed — so the user's move
  simply vanished.

  A **JMAP** server materializes the override itself from a `recurrenceOverrides/<start>/…`
  pointer and needs neither. So this is a CalDAV *requirement*, documented on
  `PatchTarget::Instance` as one — not a promise the neutral contract makes.
- **A move must not silently convert the value's form.** The new `DTSTART`/`DTEND` must
  be zoned in the same zone / floating / all-day as the value it replaces, or the patch
  is an `Err`. Resolving a zoned event to an instant and writing back UTC moves it for
  every reader in another zone; writing an all-day event as timed turns a day into an
  instant. Shift the **wall clock** of the event's current start (the projection
  preserves the zone and the all-day flag — that is what it is for), never a resolved
  instant. An all-day `DTEND` is **exclusive**: a one-day event on the 1st ends on the
  2nd.
- **Staged: `THISANDFUTURE`.** Splitting a series at a point (this occurrence and all
  later ones) needs the master's `RRULE` rewritten with an `UNTIL` and a second master
  minted — a different operation from overriding one instance, and not implemented. A
  host offering the usual three-way "this / this and following / all events" prompt has
  the first and third today.

## Supported recurrence subset

The model stores recurrence structurally (all of RFC 5545 `RRULE`), but the
`engine-recurrence` expander implements a subset and **rejects** the rest with a
typed error so a caller can preserve the master event without silently dropping
instances (the crate docs are the authoritative list). Consumers must treat an
expansion error as "store the event, materialize no occurrences for it (yet)",
not as a hard failure.

**But they must not treat it as nothing, either — report it.** An event that expands to
zero occurrences is stored and readable through `Engine::events`, yet it is invisible to
every *range* read, so a host laying out a grid renders it **nowhere**. It does not look
wrong; it is simply absent, with nothing anywhere saying why. So both paths that expand
carry the refusals out by key and reason — `CalendarSyncReport::unexpandable` and
`HorizonExpansion::unexpandable` — and a host is expected to surface them ("this event
can't be shown") rather than drop them. Discarding the error at the call site (an
`if let Ok(..)`) is the bug this exists to prevent.

Implemented: `FREQ` ∈ {`DAILY`, `WEEKLY`, `MONTHLY`, `YEARLY`}; `INTERVAL`;
`COUNT`/`UNTIL`/unbounded (the unbounded case capped by the horizon); `BYDAY`
including an nth-of-period (e.g. last Friday) for `MONTHLY`, and for `YEARLY` when
scoped by `BYMONTH`; `BYMONTHDAY` including negatives; `BYMONTH`; `WKST`; and
per-instance overrides (exclusion, cancellation, a moved `start`/`duration`, and
an RDATE-like addition on a non-rule instant). Every event — recurring or not —
materializes occurrences, so time-range search matches single events too.

Staged (return an error, not expanded): `BYYEARDAY`, `BYWEEKNO`, `BYSETPOS`,
year-relative nth `BYDAY`; sub-daily frequencies; `RSCALE` (preserved, never
expanded, per above); custom/embedded-`VTIMEZONE` zones (the iCalendar parser
landed with `provider-caldav` — `caldav.md` — and an IANA `TZID` is resolved
where present, but feeding a genuinely custom embedded `VTIMEZONE` into the
expander is still staged); and cross-object master/override-instance
reconciliation (the expander is a pure single-`Event` function — a recurring
master expands its inline overrides, a standalone override-instance object
expands to its own occurrence; deduplicating a master against sibling override
objects is the sync layer's job).

## Required tests

- A `VTIMEZONE` that disagrees with IANA for the same `TZID` expands using the
  documented source, and the chosen source is recorded.
- A tzdata version bump re-expands affected occurrences and leaves unaffected
  ones byte-stable.
- A floating event resolves to different instants under two host zones; an
  all-day event is zone-invariant.
- iMIP `REQUEST` → `REPLY` → `CANCEL` reconcile by `UID`/`SEQUENCE`/
  `RECURRENCE-ID`; a stale lower-`SEQUENCE` `REQUEST` does not override a newer
  one.
- A scheduling message whose `ORGANIZER` mismatches the authenticated sender is
  not auto-applied.
- **An RSVP reaches the organizer on a real auto-schedule server.** Two accounts,
  one invitation, and the assertion is on the *organizer's own copy* after the
  attendee patches their `PARTSTAT` and `PUT`s it back. No fake can stand in: the
  claim is about what a server does to a second account's resource. Verified to
  fail for the right reason — stub the patcher to store the document unchanged and
  the `PUT` still succeeds while the reply never arrives. The **neutral verb**
  (`Provider::rsvp_event`) has its own scenario beside it, because a green
  primitive says nothing about the adapter's address resolution, document patching
  and guard assembly.
- **An RSVP reaches the organizer over JMAP too — but only because the request asks.**
  On JMAP, scheduling is **opt-in per `/set`**: `sendSchedulingMessages` (default
  **`false`**) is what makes the server derive the iTIP message from the change.
  Omit it and the answer is stored and goes nowhere — the user answers, their own
  calendar agrees, nobody hears, and nothing reports a failure. Pinned from both
  sides in `provider-jmap/tests/live_calendar_scheduling.rs`
  (`jmap_rsvp_reaches_the_organizer`, `jmap_a_quiet_answer_reaches_nobody`), plus
  the write verbs' half (`jmap_cancelling_a_meeting_reaches_the_attendee`).
- ⚠️ **This doc asserted the opposite for months, and the mistake is instructive.**
  It said Stalwart did not schedule from a JMAP answer. It does. `provider-jmap`
  never sent `sendSchedulingMessages`, so the server was correctly told to notify
  nobody, and a live test recorded our own omission as *server* behaviour — because
  the only request shape it ever sent was the one the adapter builds. The CalDAV
  control arm made it worse: it "worked", which looked like proof the difference was
  server-side, when CalDAV auto-schedules per RFC 6638 and has no equivalent opt-in,
  so the two arms were never comparable. **The rule:** a live test that asserts an
  absence must first prove the absence is not caused by something we failed to send.
  History in #102, which inverts #93.
- **A real server-authored invitation parses end to end**, with its Windows `TZID`
  quoted and QP-escaped, its calendar part three levels down a `multipart/mixed`
  tree and dispositioned as an attachment, and its `ATTENDEE` folded mid-`mailto:`.
  Hand-written iMIP fixtures are all guesses about the shape; keep at least one
  captured one.
- A CalDAV event carrying properties absent from JSCalendar round-trips via
  raw-plus-patch without dropping them.
- **Patching one property of a real resource changes only that property's bytes.** The
  fixture is a recurring, multi-attendee, alarmed, zoned event with folded lines, `X-`
  properties, an embedded `VTIMEZONE` and non-ASCII text; the assertion is structural
  (strike the patched properties from both documents; the remainder must be
  byte-equal), because "the new value is in the document" also passes for a patcher that
  deleted the `RRULE` on its way.
- Moving one occurrence of a series leaves the master `VEVENT` byte-for-byte intact, and
  the patched resource re-reads as the same series carrying one overridden instance.
- A `DTSTART` supplied in the wrong form (UTC for a zoned event, timed for an all-day
  one) is refused, not converted.
