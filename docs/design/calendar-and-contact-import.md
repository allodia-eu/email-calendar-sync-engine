# Design note: calendar and contact import

Status: proposed with its first adapter. The calendar half (CalDAV) lands with this note; the
contact half (CardDAV) lands on top of it. Both follow `providers.md` → "Import", which said
calendar and contact import "come with their first adapter": this is that adapter, so this note
designs the seam.

## Problem

A transfer host moves a calendar between providers as the source had it: every event, with its
overrides, alarms, attendees and their answers, and nobody told. A sync host does the same with a
user's `.ics` or `.vcf` file, and a direct host with an uploaded one. Today none of them can:

- The engine writes events only as a user's change. `create_event` builds a new document from an
  `EventDraft` (no alarms, no overrides, no other people's answers), and `put_event` stores a
  document as it is but schedules it: on a server that auto-schedules (RFC 6638), storing a
  meeting sends its invitations.
- Nothing creates a calendar or an address book. `caldav.md` said hosts provision calendars out of
  band; `contacts.md` deferred address-book creation.
- Nothing answers "this event is already there" by its `UID`, so a retried import, or one into a
  calendar that already holds the event, either fails or stores it twice.

## Survey

Every request below was sent to the harness servers (Stalwart 1.0.2, SabreDAV 4.7.0 and
Nextcloud 35.0.2) on 2026-11-23, and the answers the importer reads are kept as fixtures in
`crates/provider-caldav/tests/fixtures/import/`. Scratch calendars and books were made for it and
removed afterwards.

### Calendars

| Question | Stalwart | SabreDAV | Nextcloud |
|---|---|---|---|
| `MKCALENDAR` with `displayname`, `calendar-description`, Apple `calendar-color`, `supported-calendar-component-set`, `calendar-timezone` | `201`, every property kept | `201`, every property kept | `201`, every property kept |
| RFC 7809 `calendar-timezone-id` | `404` | `404` | `404` |
| `MKCALENDAR` on an existing path | `405` | `405` | `405` |
| `max-resource-size` (RFC 4791 §5.2.5) | 524,288 | 10,000,000 | 10,000,000 |
| `max-attendees-per-instance`, `min/max-date-time`, `max-instances` | 20, 0001–9999, 3,000 | not stated | not stated |
| `PUT` + `If-None-Match: *` on a new name | `201` + `ETag` | `201` + `ETag` | `201` + `ETag` |
| The same again | `412`, empty body | `412`, `PreconditionFailed` | `412`, `PreconditionFailed` |
| A second object with a held `UID`, same calendar | `403` `no-uid-conflict` naming the holder | **`201`: a duplicate** | `409` `no-uid-conflict` naming the holder |
| The same `UID` in another calendar | `201` | `201` | `201` |
| A `VTODO` into a `VEVENT`-only calendar | `403` `supported-calendar-component` | `403` | `403` |
| An object with no component | **`412`** `valid-calendar-object-resource` | `403` `supported-calendar-component` | `403` `supported-calendar-component` |
| Not iCalendar at all | **`412`** `valid-calendar-object-resource` | `415` | `415` |
| `calendar-query` with a `UID` `text-match` | `207` | `207` | `207` |
| ... matching a substring of a `UID` | yes (§9.7.5 says substring) | yes | yes |
| Partial `calendar-data` (only `UID`) on a listing | honoured | ignored: whole objects | ignored: whole objects |
| How an object `PUT` to `uid%40example.com.ics` is listed afterwards | `uid@example.com.ics` | `uid@example.com.ics` | `uid@example.com.ics` |
| Time per `UID` lookup, calendar of 1,000 | ~10 ms | ~65 ms, linear in the calendar | ~390 ms, the same as on an empty calendar (per-request cost) |

The two bold departures are recorded in `docs/spec-deviations.md` (SABREDAV-1, STALWART-2).

### Scheduling

`SCHEDULE-AGENT=CLIENT` (RFC 6638 §7.1) on every `ORGANIZER` and `ATTENDEE` tells a server that
the client schedules for that party, so the server sends nothing. Proven on Stalwart, the one
harness server that schedules, between its two scratch accounts
(`tests/scheduling/import.rs`): Bob storing a meeting that invites Carol, as a plain create, puts it
on Carol's calendar, in her scheduling inbox and in her mailbox (the control); the same meeting
imported reaches none of the three in five times the control's delivery time. With the injection
removed the import arm fails. Spike S3 of the transfer host ran the same check over 200 meetings
on all three servers, SabreDAV and Nextcloud with scheduling turned on, and every server kept the
parameter in the stored object.

The parameter is permanent: the server does not schedule that meeting later either, when an
organizer edits it in a client that keeps the parameter. A host says so to its users. A client that
drops the parameter on save gets server scheduling back.

### Contacts

| Question | Stalwart | Nextcloud |
|---|---|---|
| Extended `MKCOL` (RFC 5689) with `displayname`, `addressbook-description` | `201`, both kept | `201`, both kept |
| `MKCOL` on an existing path | `405` | `405` |
| `supported-address-data` | vCard 4.0, 3.0, 2.1 | vCard 3.0, 4.0, jCard |
| `max-resource-size` | 524,288 | 10,000,000 |
| A vCard 3.0 `PUT` | stored, read back as 4.0 | stored as sent; read back as 3.0, or as 4.0 when asked |
| A rich vCard 4.0 (26 properties, `PHOTO` data URI, `X-` property, `RELATED`, `GEO`) | every property kept, values re-serialized (`TYPE` upper-cased) | stored as sent |
| A second card with a held `UID` | `403` `no-uid-conflict` naming the holder | `409` `no-uid-conflict` |
| Not a vCard | **`412`** `supported-address-data` | `415` |

The SabreDAV fixture serves no CardDAV.

## Design

### The seam

Two traits behind the `import` feature, `engine_provider::import::{CalendarImporter,
ContactImporter}`, and one verb per container outside it, `CalendarCollections::create_calendar`
and `AddressBookCollections::create_address_book`. Each is reached through an accessor on
`Provider` that defaults to `None` and is delegated by `Box<dyn Provider>` (north-star principle
3), like `Provider::mail_importer`:

```rust
#[cfg(feature = "import")]
fn calendar_importer(&self) -> Option<&dyn CalendarImporter> { None }
fn calendar_collections(&self) -> Option<&dyn CalendarCollections> { None }
```

`ContactsProvider` is a `Provider`, so a host holding either reaches the contact accessors the same
way.

### What mirrors mail, and what does not

| | Mail (`MailImporter`) | Calendars (`CalendarImporter`) | Contacts (`ContactImporter`) |
|---|---|---|---|
| Item | raw RFC 5322 + mailboxes + `MessageMetadata` | raw iCalendar: one event with its overrides, + its calendar | raw vCard + its address book |
| Answers | one per item, in order | the same | the same |
| `Ok` | stored, or already there (JMAP `alreadyExists`) | stored, or already there **by `UID`** | stored, or already there **by `UID`** |
| `requires_confirmation` | sent, unanswered | the same: the `PUT` is the point of no return | the same |
| `item_unavailable` | too large, too many mailboxes, refused | too large, not one event, refused | too large, a vCard version the book does not take, refused |
| `kept` | per field of `MessageMetadata` | none yet | none yet |
| Limits, one `check` | largest message, mailboxes per message | largest object (`max-resource-size`), per calendar | largest card and the vCard versions, per book |
| Cursor before the first write | per mailbox | per calendar (`sync-token`, or the CTag) | per address book (the same) |

Where they differ, the data decides:

- **No `kept` for an event or a card yet.** A message carries state beside its bytes (keywords, an
  arrival time) that a target keeps or drops field by field. An event's alarms, attendees and
  recurrence, and a card's every property, are *inside* the document, which CalDAV and CardDAV
  store whole. Every surveyed server read back what it was given (the projection, for events; every
  property, for the rich card). A target that keeps less is the case `kept` exists for, so it is
  added with the first adapter that has one: a Graph or Google target, or the vCard writer of E9.
  The receipts are `#[non_exhaustive]`, so adding it breaks no caller.
- **Identity is the `UID`.** A message is matched by fingerprint, by the host. An event and a card
  name themselves, so the importer answers "already there" for a `UID` the container holds and
  never stores a second.
- **Silence is the contract, not an option.** A mail import tells nobody by nature. A calendar
  import must suppress scheduling, so the trait promises it and each adapter says how. CalDAV:
  `SCHEDULE-AGENT=CLIENT`. A future JMAP adapter: `sendSchedulingMessages: false`. Google: its
  `events.import`. Graph has no switch; a Graph importer would have to keep less (drop the
  attendees, reported in `kept`) or not exist. There is no flag: a target that cannot import
  silently does not offer the trait.
- **Limits are per container**, because CalDAV and CardDAV state them per collection.

### The shared vocabulary

What crosses hosts and transports, and so lives in `engine-core`: the item formats (`RawIcal`,
`RawVcard`), the ids (`CalendarId`, `EventId`, `AddressBookId`, `ContactId`), `RevisionTokens`,
the containers (`Calendar`, `AddressBook`), and, for contacts, `VcardVersion`, which the limits and
the card share. The import request and receipt types sit beside `MessageImport` in
`engine_provider::import`, as the mail ones do.

### Containers

The engine had no calendar or address-book creation, so there is nothing to reuse and one path is
built. It is not an import verb: a sync host creating a calendar for its user is the same act. So
it is a capability of its own outside the `import` feature, which a transfer host and a sync host
share, as mail import shares `MailboxWrites::edit_mailbox` with folder edits.

- `create_calendar(account, &NewCalendar) -> Calendar`: name, description, colour. The answer is
  the calendar read back with the listing's own `PROPFIND`, so it is the receipt of what was kept.
- Always a new container: a name is not an identity. The path segment is a fresh 128-bit token, so
  a lost answer is "may be stored", and the host finds the calendar in the list.
- **No time zone yet.** A CalDAV calendar's time zone is a `VTIMEZONE` definition
  (`calendar-timezone`); no harness server takes RFC 7809's bare id. Building one from tzdb is the
  iCalendar serializer's job (E7), so `NewCalendar` gains `time_zone` with it, rather than take an
  input no adapter can honour. The events carry their own `VTIMEZONE`s, so they are unaffected.
- **No component set.** The engine models events alone, and every surveyed server's default set
  holds them; a `VEVENT`-only set would refuse a task a client adds later.

### The CalDAV importer

Per event:

1. `engine_ical::import_object` (one implementation of "prepare a calendar object to be stored
   silently", in the iCalendar crate): checks the object is one event with one `UID`, adds
   `SCHEDULE-AGENT=CLIENT` to every `ORGANIZER` and `ATTENDEE` outside an alarm, replaces another
   `SCHEDULE-AGENT`, drops `SCHEDULE-FORCE-SEND` and a `METHOD` (RFC 4791 §4.1), and keeps every
   other byte.
2. The calendar's limits (one `PROPFIND` per calendar per batch) and the one `check`.
3. **A `UID` lookup** (`calendar-query`, exact match of the substring results), on every server.
   SabreDAV stores a duplicate and nothing distinguishes it from a server that refuses one, so the
   lookup cannot be skipped where it is needed; the measured cost is in the survey. A server that
   will not answer the query (`4xx`) leaves the `PUT` to its own refusal.
4. `PUT` with `If-None-Match: *` to the name minted from the `UID` (`CalDavProvider::event_href`,
   the one rule every create uses). `412` with no CalDAV precondition: an earlier attempt of this
   import stored it. `403`/`409` with `no-uid-conflict`: already there, under the href the refusal
   names. A CalDAV precondition, `400`, `413` or `415`: `item_unavailable`. A gateway's `502`/`504`
   or a lost answer: `requires_confirmation`.

Two findings changed the shared create path too:

- **An object's `@` is left unencoded in its name**, since all three servers list
  `uid@example.com.ics` for a `PUT` to `uid%40example.com.ics`. Minted the way it is listed, the id
  a write answers is the id every later read reports.
- **A `UID` longer than 100 encoded bytes is named by its SHA-256.** Meeting `UID`s from Outlook
  are long, and SabreDAV's MySQL schema keeps a name in 200 bytes.

Errors on these paths carry a class and an HTTP status, never the server's text
(`CalDavError::content_free`): an error body can echo an event's summary.

## Three-host check

| | Sync host | Transfer host | Direct host |
|---|---|---|---|
| What it imports | a user's `.ics` / `.vcf`, into a calendar or book it syncs | every event and card of a source calendar or book | an uploaded file, into the user's calendar or book |
| Containers | creates a calendar or book when the user asks | creates one per source container, then imports into it | rarely; the verb is there |
| `UID` already there | linked: importing a file twice stores nothing twice | linked: a retried or taken-over batch stores nothing twice | linked |
| Cursor | not needed (the outbox and sync reconcile) | recorded before each batch's first write; a takeover reads the delta from it | not needed |
| Silence | importing a file of meetings sends no invitations | the whole point (safety invariant 1 of the transfer host) | the same as the sync host |
| What it still needs | a splitter for a multi-event `.ics` and a multi-card `.vcf` (one object per item here); the outbox op if the import should be durable | nothing more for CalDAV/CardDAV targets; E7 for Graph sources; E9 for converting cards | the splitter |

## Graph and Google later

- **Google Calendar** has `events.import`, documented to store a private copy of an event by its
  iCalendar `UID` without sending invitations: it maps onto `CalendarImporter` as it stands, with
  a `kept` for what its JSON cannot carry, and its cursor would be a `syncToken`. Not surveyed
  live; its duplicate-`UID` answer is to be measured when the adapter is built.
- **Graph** can create an event but not silently; a Graph `CalendarImporter` would have to import
  without attendees and say so in `kept`, or not be offered. Contacts import over Graph and Google
  People converts the vCard first (E9).
- **JMAP** `CalendarEvent/set` with `sendSchedulingMessages: false` and `ContactCard/set` (or
  `ContactCard/parse`, RFC 9610) fit the same traits.

None of these is implemented; the types leave room for them without a breaking change.

## Breaking changes

None to a public API: the new traits, types and accessors are additive, and every accessor has a
default. One behaviour changes: a CalDAV create names an object with its `UID`'s `@` unencoded,
and a very long `UID` by its hash, so the id in the receipt is the id sync reports.

## Open questions

- **The `UID` lookup's cost on SabreDAV grows with the calendar** (its backend parses every object
  per query). A host that knows the target is empty, or already compared `UID`s, pays it anyway.
  A per-batch listing does not help: SabreDAV and Nextcloud ignore partial retrieval and would send
  every object each batch.
- **The attendee's direction of the silence proof** has no control on Stalwart: Carol storing her
  accepted copy plainly made Stalwart send Bob no `REPLY` within 30 s (S3 saw the same), so the
  import arm would prove nothing there. A SabreDAV fixture with its scheduling plugin (as S3 ran)
  would give that arm a control.
- **A multi-event `.ics` splitter** belongs in `engine-ical` for the sync and direct hosts.
- **A calendar's time zone** waits on E7's `VTIMEZONE` builder.
