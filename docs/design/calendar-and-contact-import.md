# Design note: calendar and contact import

Status: proposed with its first adapter. The calendar half (CalDAV) lands with this note; the
contact half (CardDAV) lands on top of it, in a change of its own. Both follow `providers.md` → "Import", which said
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
| `addressbook-query`, `UID` `text-match` with `match-type="equals"` | exact | exact |
| How a card `PUT` to `uid%40example.com.vcf` is listed | `uid@example.com.vcf` | `uid@example.com.vcf` |

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
  added with the first adapter that has one: a Graph or Google target, or a vCard writer that converts between versions (not yet in the engine).
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
  job of an event-to-iCalendar writer that builds `VTIMEZONE` from tzdb (not yet in the engine), so `NewCalendar` gains `time_zone` with it, rather than take an input no adapter
  can honour. The events carry their own `VTIMEZONE`s, so they are unaffected.
- **No component set.** The engine models events alone, and every surveyed server's default set
  holds them; a `VEVENT`-only set would refuse a task a client adds later.

### The CalDAV importer

Per event:

1. `engine_ical::import_object` (one implementation of "prepare a calendar object to be stored
   silently", in the iCalendar crate): checks the object is one event with one `UID` (every
   component carrying a `UID` must be an event directly in the calendar), adds
   `SCHEDULE-AGENT=CLIENT` to every `ORGANIZER` and `ATTENDEE` outside an alarm (a group-prefixed
   one too), drops every other `SCHEDULE-AGENT` spelling, `SCHEDULE-FORCE-SEND` and a `METHOD`
   (RFC 4791 §4.1), and keeps every other byte. **It fails closed**: components that do not
   nest, a party line it cannot parse (an unbalanced quote) or a party outside every component
   refuse the object, and the rewritten text is parsed again and checked before it is answered.
   Which component a line is in is read off the read path's own component tree, by line index.
2. The calendar's limits (one `PROPFIND` per calendar per batch) and the one `check`, on the
   rewritten text: the size checked is the size stored.
3. **A `UID` lookup** (`calendar-query`, keeping only results whose event's `UID` is ours, read
   with `engine_ical::object_uid`), on every server. SabreDAV stores a duplicate and nothing
   distinguishes it from a server that refuses one, so the lookup cannot be skipped where it is
   needed; the measured cost is in the survey. A lookup the server refuses (a `Permanent` answer)
   leaves the `PUT` to its own refusal; a throttle, a refused credential or a server error answers
   the event, since an unanswered lookup is not one that found nothing.
4. `PUT` with `If-None-Match: *` to the name minted from the `UID` (`CalDavProvider::event_href`,
   the one rule every create uses). A bare `412`: the name is read (`GET`), and the event is
   already there only when the object there has our `UID`; another is a `Conflict`.
   `403`/`409` with `no-uid-conflict`: already there, under the href the refusal names.
   Everything else is judged by the one write classifier (`CalDavError::write_refusal`), which the
   sync create path reads too:

   | Answer | Meaning | What the import says |
   |---|---|---|
   | a CalDAV or CardDAV object precondition (`403`, or Stalwart's `412`, STALWART-2); `400`, `413`, `415` | the object | `item_unavailable` |
   | `507`, `DAV:quota-not-exceeded` | the target's storage | `refused_by_target(StorageFull)` |
   | `404`; `409` naming no precondition (RFC 4918 §9.7.1; Stalwart's answer for a missing calendar) | the calendar is not there | `refused_by_target(NoSuchContainer)` |
   | `403` naming `need-privileges` (SabreDAV); a plain `403` (Stalwart's read-only share, Nextcloud's birthday calendar) | no permission | `refused_by_target(ReadOnlyContainer)` |
   | Stalwart's `400` concurrency refusal | a throttle | `RateLimited` |
   | a gateway's `502`/`504`, or a lost answer | unknown | `requires_confirmation` |

   A plain `403` is read as no permission, not as the object's refusal, on that evidence: both
   servers that answer one answer it for a calendar the account may not write, and an object's
   refusal names its precondition.

Two findings changed the shared create path too:

- **An object's `@` is left unencoded in its name**, since all three servers list
  `uid@example.com.ics` for a `PUT` to `uid%40example.com.ics`. Minted the way it is listed, the id
  a write answers is the id every later read reports (the live suite asserts it).
- **A name longer than 255 bytes is replaced by the `UID`'s SHA-256.** Measured: Nextcloud 35 on
  PostgreSQL stores a 255-byte name and answers a 260-byte one `500`, which would read as
  retryable; Stalwart and the SabreDAV fixture took 1,004 bytes. An Outlook meeting `UID`, about
  112 characters, is far inside the limit.

### Where silence is promised

`Provider::calendar_importer` answers the CalDAV importer where silence is known, and only there:

| What the server says | Importer | Why |
|---|---|---|
| The calendar home advertises `calendar-auto-schedule` | `Some` | RFC 6638 §2 has a scheduling server say so, and §7.1 binds it to leave a `SCHEDULE-AGENT=CLIENT` party to the client. Proven live on Stalwart with a control that does deliver, and in the transfer host's spike on SabreDAV and Nextcloud with scheduling turned on. |
| No `calendar-auto-schedule`, and the principal has no `schedule-inbox-URL` or `schedule-outbox-URL` (RFC 6638 §2.2) | `Some` | Nothing can be sent: the server does no scheduling. The SabreDAV fixture (observed: its principal answers both properties `404`). |
| No `calendar-auto-schedule`, but a scheduling inbox or outbox | `None` | The server can schedule and does not say whether it will; it may schedule without advertising it (a departure from §2). |
| No `calendar-auto-schedule`, and the home was found without asking the principal (a lenient well-known) | `None` | Unknown. |

The inbox and outbox are read off the principal by the discovery `PROPFIND` that already asks it
for the calendar home, so the rule costs no request. The importer is the same code everywhere.

Google's CalDAV endpoint is recorded as unsurveyed (`docs/spec-deviations.md`, GOOGLE-2): no
credential for the test account was available in this environment, so whether it advertises
scheduling, lists scheduling collections, and honours the parameter is not known. The engine does
not tell a Google account apart (`provider-caldav` copes with servers by what they answer, never
by host name), so the rule above decides for it on what its principal says: `None` if it lists a
scheduling inbox or outbox without advertising scheduling.

Errors on these paths carry a class and an HTTP status, never the server's text
(`CalDavError::content_free`): an error body can echo an event's summary.

### The CardDAV importer

The calendar importer's steps, read through the same module (`dav_import`: what a `PUT`, a `UID`
lookup and a collection's properties answer), with what a card changes:

- The card is stored byte for byte: a vCard schedules nothing.
- The book's limits add its vCard versions (`supported-address-data`); a card in another, or one
  stating none, is `item_unavailable` before anything is sent. Converting is E9's.
- The lookup is `addressbook-query` with `match-type="equals"`, its answer still checked. Both
  harness servers refuse a second `UID` themselves, so on them the lookup only saves a `PUT`; it
  is there for a server that does not (as SabreDAV's backend does not for events), which the
  harness cannot show for cards.
- A card without a `UID` (optional in vCard 3.0) is named by its bytes' hash, so a retry meets
  its own write; it cannot be looked up. Read back after a bare `412`, any one card at that name
  is taken as ours: the servers rewrite what they store (Stalwart writes 3.0 as 4.0, Nextcloud
  gives the card a `UID` of its own, both found live), and only those bytes lead to the name.
- The name is the event's rule (`href::resource_name`): both servers list the `@` raw.
  `create_contact` names a card the same way, and a card with no `UID` by a SHA-256 of its name
  and addresses (no longer `DefaultHasher`, which is not stable across Rust releases).
- `address-data-type` takes RFC 6352 §10.3's defaults: no `content-type` is `text/vcard`, no
  `version` is 3.0; the media type is compared without its parameters.
- A bare `412`, a refusal, a throttle and the lookup's answers are read as for events
  (`dav_import`, `write_refusal`).

## Three-host check

| | Sync host | Transfer host | Direct host |
|---|---|---|---|
| What it imports | a user's `.ics` / `.vcf`, into a calendar or book it syncs | every event and card of a source calendar or book | an uploaded file, into the user's calendar or book |
| Containers | creates a calendar or book when the user asks | creates one per source container, then imports into it | rarely; the verb is there |
| `UID` already there | linked: importing a file twice stores nothing twice | linked: a retried or taken-over batch stores nothing twice | linked |
| Cursor | not needed (the outbox and sync reconcile) | recorded before each batch's first write; a takeover reads the delta from it | not needed |
| Silence | importing a file of meetings sends no invitations | the whole point (safety invariant 1 of the transfer host) | the same as the sync host |
| What it still needs | a splitter for a multi-event `.ics` and a multi-card `.vcf` (one object per item here); the outbox op if the import should be durable | nothing more for CalDAV/CardDAV targets; an event-to-iCalendar writer that builds `VTIMEZONE` from tzdb (not yet in the engine) for Graph sources; a vCard writer that converts between versions (not yet in the engine) for converting cards | the splitter |

## Graph and Google later

- **Google Calendar** has `events.import`, documented to store a private copy of an event by its
  iCalendar `UID` without sending invitations: it maps onto `CalendarImporter` as it stands, with
  a `kept` for what its JSON cannot carry, and its cursor would be a `syncToken`. Not surveyed
  live; its duplicate-`UID` answer is to be measured when the adapter is built.
- **Graph** can create an event but not silently; a Graph `CalendarImporter` would have to import
  without attendees and say so in `kept`, or not be offered. Contacts import over Graph and Google
  People converts the vCard first, with a vCard writer that converts between versions (not yet in the engine).
- **JMAP** `CalendarEvent/set` with `sendSchedulingMessages: false` and `ContactCard/set` (or
  `ContactCard/parse`, RFC 9610) fit the same traits.

None of these is implemented; the types leave room for them without a breaking change.

## Breaking changes

None to a public API: the new traits, types and accessors are additive, and every accessor has a
default. Behaviour that changes on existing paths:

- a CalDAV create names an object with its `UID`'s `@` unencoded, and a `UID` whose name would
  pass 255 bytes by its hash, so the id in the receipt is the id sync reports;
- a write Stalwart refuses with `412` naming a precondition is `Permanent`, no longer the
  `Conflict` a bare `412` is.

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
- **A calendar's time zone** waits on an event-to-iCalendar writer that builds `VTIMEZONE` from tzdb (not yet in the engine).
- **A short `UID` costs more to look up.** `text-match` matches substrings (RFC 4791 §9.7.5),
  so a short `UID` such as `1` returns every object whose `UID` contains it, each one read to
  compare; importing a calendar of such `UID`s grows with the square of its size.
- **Google's CalDAV is unsurveyed** (GOOGLE-2): a live check with the Google test account is
  needed before a host imports into it.
- **No live CardDAV server without `UID` enforcement.** The card lookup runs live (a re-import
  and another client's card are found by it), but no harness server would store the duplicate
  without it; adding CardDAV to the SabreDAV fixture would show that.
