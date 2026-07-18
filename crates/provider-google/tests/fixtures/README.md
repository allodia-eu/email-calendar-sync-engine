# provider-google test fixtures

Real Google (Gmail **v1** + Calendar **v3**) JSON responses captured from a throwaway
account via `tools/google-oauth`, then **scrubbed** of PII. The object *shapes* are
verbatim from the live API; only account-identifying values were mapped to deterministic
fakes, consistently, so cross-references (thread ids, label ids, Message-IDs, sync
tokens) survive:

- emails → `testuser@example.test`, display name → `Test User`
- Gmail message ids → `message-N`; thread ids follow (a thread's root shares the root
  message's id, exactly as Gmail returns it)
- Gmail-assigned `Message-Id` headers → `message-N@mail.gmail.test`
- opaque sync/page tokens → `…-sync-token-N` (they base64-encode account state incl. the
  email); a calendar `htmlLink`'s `eid` (which base64-encodes the email) → a placeholder
- the Meet `hangoutLink`/`conferenceData` join credential → a placeholder (`aaa-bbbb-ccc`);
  a public repo must not ship a working join link

The scrub is reproducible: `scratchpad/scrub.py` + `scrub_cal.py` (kept out of the repo)
map the gitignored raw captures to these files. The Gmail message fixtures are
deterministic self-sent messages ("Fixture: …"); the calendar fixtures are events created
on the account.

## Gmail files (`mail/`)

| Fixture | Real call | Protects |
| --- | --- | --- |
| `mail/profile.json` | `GET /gmail/v1/users/me/profile` | the account cursor (`historyId`) a snapshot persists |
| `mail/labels.json` | `GET /users/me/labels` | label → `Mailbox` role/keyword/membership mapping (system + a custom label) |
| `mail/messages_list.json` | `GET /users/me/messages` | the `{id, threadId}` enumeration a snapshot pages |
| `mail/message_metadata.json` | `GET /users/me/messages/{id}?format=metadata&metadataHeaders=…` | envelope/labels/thread normalization (unread, single-membership) |
| `mail/message_metadata_labeled.json` | same, the labeled reply | **multi-membership** (INBOX+SENT+custom) + `STARRED`/`IMPORTANT` keywords, read, threaded to the first |
| `mail/message_full.json` | `GET …/messages/{id}?format=full` | the `payload` tree (`body.data`, parts) + attachment detection |
| `mail/message_raw.json` | `GET …/messages/{id}?format=raw` | base64url `raw` → `RawMime` decode |
| `mail/history_delta.json` | `GET /users/me/history?startHistoryId=…` | delta shape: `messagesAdded`/`labelsAdded`/`labelsRemoved` (partials → re-fetch) |
| `mail/history_deleted.json` | same, after a permanent delete | `messagesDeleted` tombstone shape |
| `error/*.json` | a 401/403(rate)/403(perm)/404/410 | `error` envelope → `FailureClass` mapping |

## Real-behavior findings (captured, not assumed)

1. **Gmail's message scope is account-global.** `historyId` is one account-wide cursor
   (JMAP-like), not per-folder (Graph) or per-mailbox (IMAP). A snapshot enumerates
   `messages.list` and persists the *profile's* `historyId`; the delta is `history.list`.
2. **Labels are multi-membership + keyword state.** A message's `labelIds` is its
   membership across labels at once. `UNREAD`/`STARRED` are keyword *state* (`$seen` is
   the **absence** of `UNREAD`; `STARRED` → `$flagged`), so they are excluded from
   membership and never emitted as mailboxes. `DRAFT` is both the Drafts place and the
   `$draft` state.
3. **Header names come back mixed-case** (`Message-Id`, not `Message-ID`), so header
   lookup is case-insensitive. `internalDate` is epoch-millis (→ `received_at`); the
   `Date` header is RFC 2822 (→ `sent_at`).
4. **History changes are partials.** `messagesAdded`/`labelsAdded`/`labelsRemoved` carry
   only `{id, threadId, labelIds}`, so each touched-but-present id is **re-fetched** full
   (the engine applies whole objects); `messagesDeleted` tombstones.
5. **`messages.send` rewrites the caller's `Message-ID`** (a captured
   `<…@example.test>` came back as `<…@mail.gmail.com>`), so reconcile-by-`Message-ID`
   would not match — but `send` **returns the sent message's id** in its response, so the
   receipt uses that directly (no reconcile needed, unlike SMTP/Graph `sendMail`).
6. **`messages.insert`/`import` require the `/upload/` endpoint** (the normal path 404s
   with HTML on both `www.googleapis.com` and `gmail.googleapis.com`). The provider never
   inserts (it uses `send`/`modify`/`trash`/`delete`, all normal-path methods), so the
   single `https://www.googleapis.com` base serves every method the adapter calls. The
   fixtures' self-sent messages therefore use `messages.send`, not `insert`.
7. **A `404` from `history.list`** (an aged-out `startHistoryId`) is Gmail's resync
   signal → `GoogleError::HistoryExpired` → snapshot restart. It has **no live test**: a
   fresh account's history window still contains an id of `1`, returning a valid (large)
   delta rather than a `404`, so the recovery is proven offline with a fake.

## Calendar files (`calendar/`)

| Fixture | Real call | Protects |
| --- | --- | --- |
| `calendar/calendars.json` | `GET /calendar/v3/users/me/calendarList` | calendar-list → `Calendar` (primary + a reader-role holiday calendar; access role, colour) |
| `calendar/events_list.json` | `GET /calendar/v3/calendars/primary/events?singleEvents=false` | the event page (masters kept with `RRULE`) + `nextSyncToken` |
| `calendar/events_delta.json` | same, replaying the `syncToken` | delta shape: an updated event + a `status:"cancelled"` tombstone + a new `nextSyncToken` |
| `calendar/event_single.json` | one timed event | single event + attendees/location/organizer |
| `calendar/event_recurring_master.json` | a `GET …/events/{id}` series master | `recurrence` `RRULE` + zoned start/end |
| `calendar/event_allday.json` | an all-day event | `start.date`/`end.date` (zoneless) |
| `calendar/event_meet.json` | a Google Meet event | `conferenceData`/`hangoutLink` (join credential scrubbed) |

8. **Google Calendar is IANA-native.** Event times are `dateTime` (RFC 3339 with offset)
   plus an IANA `timeZone` (e.g. `Europe/Amsterdam`) — no Windows-zone table (contrast
   Graph). Recurring events return as **masters with an RFC 5545 `RRULE`**
   (`singleEvents=false`), the master+rule+local-expansion model the engine wants.
9. **`events.list` returns a `nextSyncToken` on the final page**; a `410` on replay means
   the token expired (→ full resync). A deleted event appears in the delta as `{id, etag,
   kind, status:"cancelled"}` (a minimal tombstone).
10. **Calendar writes are `If-Match`-guarded** (`events.insert`/`patch`/`delete`); a stale
    ETag is a `412 conditionNotMet`. `events.patch` merges a partial event and echoes the
    updated one with a new `etag` (the ETag advances on every write).
11. **Delete idempotency differs from Graph.** Google signals *already-gone* as `404` **or
    `410 Gone`** (both are treated as idempotent success). A `delete` leaves the event
    **cancelled with a new ETag**, so a *guarded* re-delete (the stale `If-Match`) returns
    `412`, not `404`/`410` — a real conflict, surfaced for the caller to refetch. The live
    test therefore does not re-delete with the old guard; the `404`/`410`-gone idempotency
    is covered offline.
