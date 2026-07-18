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
| `wellknown/*.json` | `GET /me/mailFolders/{inbox,drafts,…}` | well-known-name → id role resolution |
| `error/bad_request.json` / `unauthorized.json` | a 400 and a 401 | `error` envelope → `FailureClass` mapping |
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

## Calendar fixtures (`calendar/`)

Captured the same way, from events created on the throwaway account. Only **PII was
scrubbed** (emails → `testuser@example.test`, name → `Test User`); the opaque Graph
object ids are kept verbatim (they are per-object handles for a throwaway account, not
PII, and keeping them preserves the real shape). Event times were captured with
`Prefer: outlook.timezone="Europe/Amsterdam"` — the authoring-zone form the live
provider requests (see Finding 6).

| Fixture | Real Graph call | Protects |
| --- | --- | --- |
| `calendar/calendars.json` / `calendar.json` | `GET /me/calendars` | calendar → `Calendar` normalization; the default calendar |
| `calendar/event_series_master.json` | `GET /me/events/{id}` (a `seriesMaster`) | `patternedRecurrence` → `Recurrence`, zone, location, organizer |
| `calendar/event_single.json` | a `singleInstance` from `GET /me/events` | non-recurring event + attendee projection |
| `calendar/event_allday.json` | an all-day `singleInstance` | `isAllDay` → zoneless `Date` + one-day duration |
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
