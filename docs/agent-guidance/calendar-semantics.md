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
- Normalization target: JSCalendar (`LocalDateTime` + IANA `timeZone`, or UTC)
  and iCalendar (`DTSTART` with `TZID`/`VTIMEZONE`, UTC `Z`, or floating) both map
  to one engine time model — an instant resolved through its zone, or wall-clock
  for floating.
- **Adapters may deliver non-IANA zones.** Microsoft Graph uses Windows zone
  names (and `tzone://Microsoft/Custom` for legacy custom zones). The adapter
  maps these to IANA at its boundary (CLDR `windowsZones`); the engine time model
  is IANA-only.
- **Out of scope:** `RSCALE` / non-Gregorian recurrence (RFC 7529) is preserved
  raw, not expanded.

## Inbound scheduling (iTIP/iMIP)

The Write Contract covers *outbound* scheduling. Inbound is the missing half:
recognizing and reconciling scheduling messages that arrive through sync.

- **iMIP is iTIP over email:** a message with a `text/calendar` part carrying a
  `METHOD`. The mail sync path must detect these and hand them to the calendar
  layer — this is the mail↔calendar bridge.
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
- **`METHOD` handling:**
  - `REQUEST` → create or update the event (needs-action by default).
  - `REPLY` → update the replying attendee's `PARTSTAT` on the organizer's copy.
  - `CANCEL` → cancel the event or the targeted instance.
  - `COUNTER` / `DECLINECOUNTER` / `REFRESH` / `ADD` → classify and surface; full
    handling may be staged.
- **Responding** is an outbox operation that separates calendar storage (my
  `PARTSTAT`) from delivery (the iTIP `REPLY` via iMIP or provider scheduling),
  consistent with the Write Contract.
- **Security.** Scheduling messages are hostile input. Validate `ORGANIZER` and
  attendee identities against the message's authenticated sender (From / DKIM /
  authenticated submission) before applying anything; never auto-apply changes
  from an unauthenticated or mismatched sender.

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
  `recurrenceRules` into one override/exclusion model; round-trips use raw.
- **Rule:** `RawIcal` and `RawJsCalendar` are preserved beside the projection
  (model invariant). Provider writes round-trip from raw plus targeted patches,
  never by re-serializing the lossy projection. The projection exists for
  display, search, and engine logic and is explicitly **not**
  round-trip-authoritative.

## Supported recurrence subset

The model stores recurrence structurally (all of RFC 5545 `RRULE`), but the
`engine-recurrence` expander implements a subset and **rejects** the rest with a
typed error so a caller can preserve the master event without silently dropping
instances (the crate docs are the authoritative list). Consumers must treat an
expansion error as "store the event, materialize no occurrences for it (yet)",
not as a hard failure.

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
- A CalDAV event carrying properties absent from JSCalendar round-trips via
  raw-plus-patch without dropping them.
