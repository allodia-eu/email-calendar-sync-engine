# provider-graph test fixtures

Real Microsoft Graph **v1.0** JSON responses captured from a throwaway personal
account (`outlook.com`) via `tools/graph-oauth`, then **scrubbed** of PII. The
object *shapes* are verbatim from the live API; only account-identifying values
were mapped to deterministic fakes, consistently, so cross-references survive:

- emails → `testuser@example.test`, names → `Test User`, user id → `00000000feedface`
- folder ids → role names (`folder-inbox`, `folder-sentitems`, …; `folder-root`
  for the `msgfolderroot` parent), message ids → `message-N`
- `conversationId`/`@odata.etag`/`changeKey`/`internetMessageId`/`conversationIndex`
  → ordinal fakes; opaque `$deltatoken`/`$skiptoken` payloads → `opaque-token-N`
- body/`bodyPreview`/`webLink` content → fixed placeholders

The scrub is reproducible: `scratchpad/scrub.py` (kept out of the repo) maps the
gitignored raw captures under `tools/graph-oauth/.local/raw/` to these files. The
3 message fixtures are deterministic self-sent messages ("Fixture: …").

## Files

| Fixture | Real Graph call | Protects |
| --- | --- | --- |
| `mail/mailfolders.json` | `GET /me/mailFolders?$top=50` | folder → `Mailbox` normalization (8 folders) |
| `mail/mailfolders_delta.json` | `GET /me/mailFolders/delta` | folder container delta + `deltaLink` cursor |
| `mail/messages_delta_snapshot.json` | `GET /me/mailFolders/inbox/messages/delta?$select=…` | **initial** sync: full message objects + `deltaLink` |
| `mail/messages_delta_nochange.json` | replay the snapshot `deltaLink` | incremental no-op (`value:[]` + new `deltaLink`) |
| `mail/messages_delta_changed.json` | replay after `PATCH isRead` | **lightweight partial** changed entry — no `@odata.etag` (see Finding 4) → re-fetched |
| `mail/messages_delta_changed_full.json` | replay after `PATCH flag` | **full** changed entry (has `@odata.etag`) → used directly, no re-fetch |
| `mail/messages_delta_removed.json` | replay after `DELETE` | `{ id, @removed:{reason} }` tombstone shape |
| `mail/messages_list_page1.json` / `_page2.json` | `GET …/messages?$top=2` + its `@odata.nextLink` | real `nextLink` pagination chain |
| `mail/message_detail.json` | `GET /me/messages/{id}` | full single-message shape (the changed-id re-fetch) |
| `mail/message_patched.json` | `PATCH /me/messages/{id}` body `{isRead,flag}` | the write echo of a mark-read + flag edit (`isRead:true`, `flag.flagStatus:"flagged"`) |
| `mail/message_moved.json` | `POST /me/messages/{id}/move` body `{destinationId}` | the move echo — **same `id`** (immutable), `parentFolderId` now the destination |
| `wellknown/*.json` | `GET /me/mailFolders/{inbox,drafts,…}` | well-known-name → id role resolution |
| `error/bad_request.json` / `unauthorized.json` | a 400 and a 401 | `error` envelope → `FailureClass` mapping |
| `mail/shared_mailbox_probe.json` | `GET /users/{shared}/mailFolders/inbox?$select=id` | the shared-mailbox probe's success shape (a **work/school** tenant, so scrubbed to `shared@example.test`) |
| `error/shared_mailbox_invalid_user.json` | the same probe, address not in the tenant | `404 ErrorInvalidUser` |
| `error/shared_mailbox_not_enabled.json` | the same probe, principal with an inactive/on-premises mailbox | `404 MailboxNotEnabledForRESTAPI` |
| `error/shared_mailbox_no_inbox.json` | the same probe, a principal that is not a mailbox (a group) | `404 ErrorItemNotFound` — "Default folder Inbox not found" |
| `error/shared_mailbox_access_denied.json` | `GET /users/{other}/mailboxSettings` **with** `MailboxSettings.ReadWrite` granted | `403 ErrorAccessDenied` — a *grant* shortfall, not a missing mailbox (see Finding below) |
| `me.json` | `GET /me` | account identity probe |

## Real-behavior findings (captured, not assumed)

1. **Personal `mailFolder` has no `wellKnownName`** (work/school-only) — selecting
   it 400s. Role mapping must resolve the well-known *aliases* (`/me/mailFolders/inbox`
   …) to ids and match, not read a role property.
2. **Folder `displayName`s are localized** (these are Dutch: "Postvak IN" = Inbox).
   Never parse display names for roles.
3. **`messages/delta` `$top` does not paginate on consumer.** Page size is
   server-controlled; `@odata.nextLink` appears only on large result sets. The
   `nextLink`-following path is therefore exercised via the *list* endpoint, whose
   `$top` does paginate.
4. **Incremental `delta` — full objects, except lightweight changes.** Per
   Microsoft's delta-query-messages doc a changed entry is a *full* object, and it
   is for substantive edits (a `flag` change → all selected fields + `@odata.etag`:
   `messages_delta_changed_full.json`). The undocumented exception, on consumer
   mailboxes, is a *lightweight* `isRead` change → only the changed property + `id`,
   **no** `@odata.etag` (`messages_delta_changed.json`). So the provider uses an
   entry with `@odata.etag` directly and **re-fetches only the etag-less partials**.
   *Snapshot* (initial) entries are always full. `@removed` items carry only `id` +
   `@removed`.
5. **Immutable ids** (requested via `Prefer: IdType="ImmutableId"`) are stable
   across calls and URL-safe — the right `ProviderKey` for Graph mail.

## Mail-write findings (captured, not assumed)

The write echoes above and the delete are the offline record of live probes against
the throwaway account (`message_patched.json` / `message_moved.json`; the delete has
no body, so no fixture).

12. **Writes are per-property, not per-keyword.** Graph has no keyword set: read/flag are
    the typed `isRead` bool and `flag.flagStatus` (`flagged`/`notFlagged`). So
    `SetKeywords` `PATCH`es `{isRead, flag}` and maps only `$seen`/`$flagged`; any other
    keyword is rejected (`$draft` is read-only). A `PATCH` echoes the updated message (200).
13. **`move` preserves the immutable id.** `POST /messages/{id}/move` returns `201` with
    the moved message carrying the **same** immutable `id` and the new `parentFolderId`, so
    the edit receipt's key is the unchanged target (the JMAP shape, not IMAP's new key).
14. **A permanent delete is `POST …/permanentDelete`, and it needs `Content-Length: 0`.**
    `DELETE /messages/{id}` only soft-deletes (to Deleted Items — that is a Trash *move*).
    The irreversible delete is `POST /me/messages/{id}/permanentDelete`, which answers `204`
    — but a bodyless `POST` **must** send `Content-Length: 0` or Graph returns
    `411 Length Required` (reqwest omits the header for an empty body, so the transport sets
    it). A re-delete of an already-purged message is `403 ErrorCannotDeleteObject`, **not** a
    clean `404` (the item lingers in Purges, still `GET`-able by id during retention); delete
    idempotency keys on `404` only, mirroring the calendar re-delete (Finding 9).

## Calendar fixtures (`calendar/`)

Captured the same way, from events created on the throwaway account. Only **PII was
scrubbed** (emails → `testuser@example.test`, name → `Test User`); the opaque Graph
object ids are kept verbatim (they are per-object handles for a throwaway account, not
PII, and keeping them preserves the real shape). Event times were captured with
`Prefer: outlook.timezone="Europe/Amsterdam"` — the authoring-zone form the live
provider requests (see Finding 6).

`calendars.json` reflects the throwaway account's real state: **two** calendars (the
default `Calendar` and a user-added `Extra calendar test`). Graph event JSON never names
its own calendar, so the provider binds each event to the calendar it was fetched under
(`Event.calendars`); that is how events from multiple calendars under one account stay
separable (see Finding 11).

The one exception to "ids verbatim" is `event_online_meeting.json`: a Teams meeting's
`onlineMeeting.joinUrl` and the meeting-id/passcodes echoed in its `body` are **live,
joinable credentials**, not opaque handles, so those were replaced with same-shape
placeholders (the meeting number, the `?p=` URL passcode, and the display passcode) — a
public repo must not ship a working join link (see Finding 10).

| Fixture | Real Graph call | Protects |
| --- | --- | --- |
| `calendar/calendars.json` | `GET /me/calendars` | calendar-list → `Calendar` normalization: **two** calendars under one account — the default `Calendar` and the non-default `Extra calendar test` (`hexColor: #f7630c`) |
| `calendar/calendar.json` | one entry from `GET /me/calendars` | a single `Calendar` (the default) in isolation |
| `calendar/event_extra_calendar.json` | a `singleInstance` from the non-default calendar's `calendarView` | an event bound to a **non-default** calendar — membership keeps it separable from the default calendar's events |
| `calendar/event_series_master.json` | `GET /me/events/{id}` (a `seriesMaster`) | `patternedRecurrence` → `Recurrence`, zone, location, organizer |
| `calendar/event_single.json` | a `singleInstance` from `GET /me/events` | non-recurring event + attendee projection |
| `calendar/event_allday.json` | an all-day `singleInstance` | `isAllDay` → zoneless `Date` + one-day duration |
| `calendar/event_online_meeting.json` | a Teams `singleInstance` from `calendarView` | the online-meeting shape (`isOnlineMeeting`, `onlineMeetingProvider`, `onlineMeeting.joinUrl`) preserved on `Event.extended` — captured ahead of online-meeting-provider support |
| `calendar/events_delta.json` | `GET /me/calendars/{id}/calendarView/delta?startDateTime=…&endDateTime=…` | the delta page shape: `seriesMaster`/`singleInstance` **kept**, `occurrence`/`exception` **dropped**, `@odata.deltaLink` cursor |

6. **Event `start`/`end` default to UTC; the authoring zone needs `Prefer:
   outlook.timezone`.** A plain `GET` returns event times in UTC, which would expand a
   recurring master DST-incorrectly. Sending `Prefer: outlook.timezone="<IANA>"` returns
   each event's wall clock in that zone (and echoes the IANA name in `timeZone`), which
   is what the provider stores. `originalStartTimeZone` carries the true authoring zone
   but not a usable wall clock, so the display-zone request is the mechanism.
7. **`calendarView/delta` returns the series master *and* its expanded occurrences.**
   The initial delta carries the `seriesMaster` (with `patternedRecurrence`), every
   pre-expanded `occurrence` (a UTC instant, `seriesMasterId` set), any `exception`, and
   standalone `singleInstance`s, ending at an `@odata.deltaLink`. The engine expands the
   master locally, so the provider keeps `seriesMaster`/`singleInstance` and drops
   `occurrence`/`exception`.
8. **Graph v1.0 exposes no `recurrenceId`/`originalStart` on an `exception`** (both are
   `null`, even on a direct `GET`), so a per-instance override cannot be keyed — hence
   exceptions are dropped, a documented limitation (`graph.md`).
9. **A re-delete of a just-deleted event is `400 ErrorInvalidRequest`**, not a clean
   `404` — the item has moved to Deleted Items. Delete idempotency keys on `404` (a
   truly-gone event); the ambiguous-retry case is the outbox's `NeedsConfirmation`.
10. **A Teams online meeting carries `isOnlineMeeting: true`, `onlineMeetingProvider:
    "teamsForBusiness"`, and an `onlineMeeting.joinUrl`** (the deprecated
    `onlineMeetingUrl` stays `null`); the join link, meeting-id, and passcodes are also
    duplicated as HTML in the `body`. The `joinUrl` **is** projected today, as an
    `Event.virtual_locations` entry. What is *not* modelled yet is the online-meeting
    **provider identity** (`onlineMeetingProvider`/`isOnlineMeeting`); that stays on the
    preserved raw payload (`Event.extended`) for a future provider-typing mapper.
11. **One MS account owns many calendars** (`GET /me/calendars` → a list). Each is a
    distinct `Calendar` with its own immutable id, `isDefaultCalendar` flag, and colour
    (`hexColor` `#rrggbb` wins over the named `color`). A Graph `event` object does not
    carry its calendar id, so calendar membership comes from the fetch context: the
    provider is calendar-bound and stamps `Event.calendars` with the calendar it synced.

## Contact fixtures (`contacts/`)

Captured from the same throwaway personal account, from a field-complete contact seeded
through `POST /me/contacts` plus one contact created by hand in Outlook (so the
`null`-vs-`""` split below is real, not constructed). Scrubbed to the same convention:
contact ids → `contact-N`, folder ids → `contact-folder-*`, `changeKey`/`@odata.etag` →
`change-key-N`, `$deltatoken` → `opaque-token-N`, addresses → `example.test`.

| Fixture | Real Graph call | Protects |
| --- | --- | --- |
| `contacts/contacts_delta_snapshot.json` | `GET /me/contacts/delta?$select=…` | **initial** sync: two full contact objects + `deltaLink`; the field-complete card exercises every `$select`ed property |
| `contacts/contacts_delta_changed.json` | replay the `deltaLink` after `PATCH jobTitle` | a changed entry — **full** object with an advanced `changeKey` (contacts have no lightweight-partial case, unlike mail Finding 4) |
| `contacts/contacts_delta_removed.json` | replay after `DELETE` | `{ id, @removed:{reason}, @odata.type }` tombstone shape |
| `contacts/contact_detail.json` | `GET /me/contacts/{id}` | the un-`$select`ed single-item shape (a **superset** of the delta fields) |
| `contacts/contact_created.json` | `POST /me/contacts` | the create echo — the `id` that becomes the write receipt |
| `contacts/contact_patched.json` | `PATCH /me/contacts/{id}` | the patch echo with the advanced `changeKey` |
| `contacts/contact_folders.json` | `GET /me/contactFolders?$select=…` | folder → `AddressBook`; `parentFolderId` is the contacts root |
| `contacts/child_folders.json` | `GET /me/contactFolders/{id}/childFolders` | the recursive-discovery leg (child's parent is the folder above) |
| `error/contacts_msa_unsupported.json` | `GET /contacts/delta` (org contacts) | the tenant-source refusal on a personal account (see Finding 15) |
| `error/contacts_directory_unauthorized.json` | `GET /users/delta` (directory) | the directory refusal on a personal account (see Finding 15) |
| `error/contacts_delta_token_bad.json` | `GET /me/contacts/delta?$deltatoken=bogus` | a malformed delta token → `400`, **not** the `410` resync signal |

15. **The tenant contact sources do not answer `403` on a personal account.** They are
    "optional, permission-gated" sources, but a personal MSA refuses them by *shape*, not
    by permission: `/contacts/delta` → **`400 BadRequest`** ("This API is not supported
    for MSA accounts"), `/users/delta` → **`401 UnknownError`** with an empty message.
    Neither is a `403`. Any degradation-to-`Unavailable` rule that keys on `403` alone
    therefore fails a personal account outright — and the `401` maps to
    `FailureClass::Authentication`, which tells a host to re-authenticate over a source
    that simply does not exist for this account type.
16. **A malformed `$deltatoken` is `400 BadRequest` ("Badly formed token"), not `410`.**
    Graph documents `410 Gone`/`resyncRequired` for an *expired* token; a syntactically
    bad one is a plain `400`. The fixtures pin the `400` shape only — an expiry-driven
    `410` cannot be forced on demand (a fresh token never ages out mid-test), so that
    recovery stays proven by the offline fake.
17. **Unset string fields are `null` on an API-created contact and `""` on a
    hand-created one.** The two captured contacts differ this way across `title`,
    `middleName`, `generation`, and `jobTitle`, so normalization must treat empty string
    and absent identically. Unset addresses are `{}` (an empty object, never `null`), and
    `homePhones` is `[]`.
18. **`birthday` comes back as a full timestamp with a non-midnight time.** A contact
    created with `"1815-12-10T00:00:00Z"` reads back as **`"1815-12-10T11:59:00Z"`** —
    Graph stores a birthday as an instant anchored near local noon, not a date. The
    engine's `Anniversary.date` is documented as *JSContact date text*, and the Google
    adapter emits `YYYY-MM-DD` there, so a verbatim copy of this string puts two
    different formats in one neutral field (and risks a day-shift when the anchor time
    crosses midnight in the reader's zone).
19. **`categories` is selected and written but never read back.** `CONTACT_SELECT` asks
    for `categories`, `contact_write` maps `ContactField::Keywords` → `categories`, and
    the captured contact really carries `["Fixture", "Engineering"]` — but
    `contact_normalize` has no `categories` branch, so keywords are lost on the way in.
    Graph advertises `ContactField::Keywords` as supported, making the round-trip lossy.
20. **Graph will not say that a mailbox exists but is not shared with you.** Probing
    every mailbox of a real tenant with `GET /users/{addr}/mailFolders/inbox` produced
    **three** distinct `404` codes and **no `403`**: `ErrorInvalidUser` (not a principal),
    `MailboxNotEnabledForRESTAPI` (inactive, soft-deleted, or on-premises), and
    `ErrorItemNotFound` — "Default folder Inbox not found" — for a principal that resolves
    but is a group rather than a mailbox. An unshared mailbox is simply a `404` too, so a
    resolver cannot distinguish "not shared with you" from "does not exist". `403
    ErrorAccessDenied` is a *different* failure — the credential's **grant** does not cover
    the route — captured on `/users/{other}/mailboxSettings` while `/me/mailboxSettings`
    answered `200` under the same scope.
21. **Percent-encoding does not contain a path traversal: Graph decodes and re-resolves.**
    `GET /v1.0/users/..%2Fme/mailFolders/inbox` answers **`200` with the signed-in user's
    own Inbox**. An address is user input, so a resolver that only escaped it would confirm
    `../me` as a "shared mailbox" and a host would onboard its own inbox under someone
    else's name. Addresses are therefore *validated* before they reach a URL
    (`principal::validate_address`); the encoding is the second layer, not the first.
