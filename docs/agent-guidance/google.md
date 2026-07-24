# Google (Gmail + Calendar) Client Guidance

This document is authoritative for the **Google provider client**
(`provider-google`) — the Gmail + Google Calendar adapter. Read it before touching
`provider-google`, the Gmail/Calendar sync paths, or the OAuth/capture tool under
`tools/google-oauth/`, alongside `providers.md` (the Provider Contract),
`store-and-sync.md` (the apply/lease model), `modeling.md`, and — for the calendar
half — `calendar-semantics.md`.

Google is the cloud-API counterpart to Graph/JMAP (OAuth bearer + JSON over HTTP),
and, like `provider-graph`, one crate houses **both** mail (Gmail) and calendar
(Google Calendar) behind one shared HTTP transport. Two facts make Google *simpler*
than Graph:

- **Gmail mail sync is account-global.** Gmail's `historyId` is one account-wide
  incremental cursor (JMAP-like), not per-folder (Graph) or per-mailbox (IMAP), so all
  of an account's messages sync under one scope — **no per-label fan-out**.
- **Google Calendar is IANA-native** and returns recurring **masters with an RFC 5545
  `RRULE`** — no Windows-zone table, no pre-expanded `calendarView` (contrast Graph).

The engine stays **OAuth-agnostic**: a host supplies a bearer access token; token
acquisition/refresh is the host's job (`north-star.md`). `tools/google-oauth` is a
standalone dev helper (outside the workspace gates) that drives the interactive
Auth-Code+PKCE loopback flow and captures fixtures — the exact mirror of
`tools/graph-oauth`.

## The crate

`provider-google` implements the `engine_provider::Provider` contract for **mail
(read/sync + on-demand source + writes + submission)** on `GmailProvider` and **calendar
(read/sync + writes)** on `GoogleCalendarProvider` (calendar-bound), each over its own
`GoogleClient` on the same token. The mail layers:

- **`error`** — `GoogleError` (`Status`/`HistoryExpired`/`Json`/`Protocol`/`Transport`)
  → the engine-neutral `FailureClass`. Google error bodies are a documented
  `{ "error": { "code", "message", "status", "errors": [{ "reason" }] } }` envelope; the
  machine `reason` (or canonical `status`) is captured. The HTTP status drives
  classification, with **one status the code alone cannot classify**: a `403` is a
  **rate limit** when its reason says so (`rateLimitExceeded`/`userRateLimitExceeded`/…)
  and an insufficient-permission **permanent** failure otherwise. `401`→auth, `409`/`412`
  →conflict, `410`→`NeedsResync` (Calendar's expired `syncToken`), `429`→rate limit,
  `5xx`→retryable.
- **`base64url`** — the URL-safe base64 codec Gmail's `raw` message field uses **both
  ways** (`send` takes it, `get?format=raw` returns it). Unlike the other adapters, which
  either assemble (encode-only, `engine-rfc5322`) or fetch raw bytes (Graph's `$value`),
  Gmail needs a codec on both sides, so it lives here with its parser.
- **`json`/`normalize`** — pure `serde_json::Value` → `Mailbox`/`Message`, unit-tested
  against captured fixtures.
- **`transport`** — a `GoogleTransport` seam over bearer HTTP. `HttpTransport` (reqwest +
  rustls, built from the caller-supplied `TlsClientConfig` passed to
  `GoogleClient::connect`/`with_base` — `tls.md`) is production; the seam lets the
  fetch/provider orchestration run offline. There is **no session discovery** (the API
  root is fixed) and **no immutable-id preference header** (unlike Graph). Like Graph,
  having no connect-time request, Google's `ConnectionInfo::http_version` is `None` until
  its first fetch, recorded at the transport's single funnel. Google returns opaque
  *tokens* (`nextPageToken`/`nextSyncToken`/`historyId`), not absolute URLs, which the
  fetch layer threads back as query params it builds itself — so, unlike Graph's `@odata`
  links, **there is no URL to rebase**; `with_base` (a proxy, a regional endpoint, or the
  test replay server) is reached because the client roots every path there.
- **`fetch`** — the label list, the message snapshot (`messages.list` → per-id
  `messages.get`), the history delta (`history.list`), and the raw-source fetch.
- **`mutate`/`submit`** — `edit_mail` (label deltas) and `submit_email` (`messages.send`).
- **`provider`** — `GmailProvider`, the `Provider` impl.

## The base URL

The single base `https://www.googleapis.com` serves **both** `gmail/v1/…` and
`calendar/v3/…`, and every method the provider calls. Note (a captured finding):
`messages.insert`/`import` are **not** on the normal path — they require the `/upload/`
endpoint and 404 with HTML on both `www.googleapis.com` and `gmail.googleapis.com`. The
provider never inserts (it uses `send`/`modify`/`trash`/`delete`), so this does not
affect it; only fixture creation avoids `insert` (it uses `messages.send`).

## Scopes

Four `SyncScope` variants (`engine-core/src/sync/scope.rs`) mirror the shape:

- **`GmailMessages { account }`** — the **account-global** message scope (cursor =
  `historyId`). Unlike `GraphFolder`, not per-label, so no cross-folder fan-out.
- **`GmailLabelList { account }`** — the label-discovery container (snapshot each pass,
  no cursor), applied before the messages that reference its labels.
- **`GoogleCalendarList { account }`** / **`GoogleCalendar { account, calendar }`** — the
  calendar-discovery container and the per-calendar event scope (cursor =
  `nextSyncToken`).

## Gmail label → model mapping (the crux)

Gmail labels drive all three of the message's independent axes (`modeling.md`):

- **Membership** = a message's `labelIds`, minus the keyword-only labels. Gmail is
  **multi-membership** (a message is in `INBOX` *and* `SENT` *and* a custom label at
  once), exactly the shape `engine-core` was built for. A message with no place label (an
  archived, uncategorized message) falls back to a **synthetic `ALL_MAIL` mailbox** so
  the non-empty `Memberships` invariant holds; the label list emits that mailbox.
- **Keywords** = `UNREAD`/`STARRED` are *state*, not a place: `$seen` is the **absence**
  of `UNREAD` (an inversion — setting `$seen` *removes* `UNREAD`), `STARRED` → `$flagged`,
  and `DRAFT` sets `$draft` (while also being the Drafts place). Keyword-only labels are
  **excluded** from membership and are **never emitted as mailboxes**.
- **Roles** (label list): `INBOX`→Inbox, `SENT`→Sent, `DRAFT`→Drafts, `TRASH`→Trash,
  `SPAM`→Junk, `IMPORTANT`→Important, plus the synthetic All-Mail→All; category/chat/
  custom labels are roleless mailboxes.

`threadId` → the provider-assigned thread (`ThreadProvenance::ProviderAssigned`), never
re-grouped by local derivation. The `Message-Id` header (mixed-case in the wire, so
lookup is case-insensitive) is preserved bracket-stripped as a threading **hint**, never
identity — the Gmail message `id` is identity. `internalDate` (epoch-millis) →
`received_at`; the `Date` header (RFC 2822) → `sent_at`.

## Sync (snapshot + history delta)

- **Snapshot** (cursor `None`): capture the account `historyId` from `profile` **before**
  enumerating, then page `messages.list`, fetching each id full (`format=metadata` with a
  fixed `metadataHeaders` set for a minimal, deterministic payload). The persisted cursor
  is that captured `historyId` (messages arriving mid-snapshot are re-reported by the
  first delta — idempotent). This is a **reconciling** pass (its present set tombstones
  absent rows). A `SyncWindow` floor windows the enumeration to `q: after:<epoch>`.
- **Delta** (cursor `Some`): `history.list?startHistoryId=…` returns
  `messagesAdded`/`labelsAdded`/`labelsRemoved` — whose message objects are **partials**
  (id + labelIds only), so every touched-but-present id is **re-fetched** full — and
  `messagesDeleted`, which tombstones. This is an **additive** pass. A `404` (the
  `startHistoryId` aged out of Gmail's window) → `GoogleError::HistoryExpired`
  (`NeedsResync`) → the stream drops the cursor and restarts as a snapshot, exactly like
  Graph's `410` restart, and only before the first page is committed. Gmail always returns
  the latest `historyId`, so the cursor always advances.

## Writes + submission

- **`edit_mail`** → `messages.modify` (label deltas), `messages.trash`, `messages.delete`.
  `SetKeywords` translates the keyword axis to state labels (the `$seen`↔`UNREAD`
  inversion; `$flagged`→`STARRED`); keywords Gmail has no label for are skipped. `MoveTo`
  honours the neutral "ends up in exactly the destination" contract (matching JMAP's
  `mailboxIds` replacement): because Gmail is multi-membership and `modify` takes deltas,
  the current place labels are fetched (`format=minimal`) and all bar the destination, the
  keyword-state labels, and the system labels `modify` cannot touch (`SENT`/`DRAFT`/`CHAT`)
  are removed while the destination is added. A `MoveTo` to `TRASH` uses `messages.trash`.
  `Delete` is a **permanent** delete past Trash — enabled by the full `mail.google.com`
  scope. A `412` is a `Conflict` the outbox resolves by refetch-and-retry.
- **`submit_email`** → `messages.send` with the whole RFC 5322 message as a base64url
  `raw` field, assembled through the shared `engine-rfc5322` (filed variant, keeping the
  `Bcc` header on the Sent copy). **Gmail rewrites the caller's `Message-ID` on send** (a
  captured finding), so reconcile-by-`Message-ID` would not match — but `send` **returns
  the sent message's id** in its response, so the receipt uses that directly (no reconcile
  round-trip, unlike SMTP/Graph `sendMail`, which return nothing).

## Google Calendar

`GoogleCalendarProvider` is **bound to one calendar** (like `GraphCalendarProvider`): its
`event_scope` names that calendar (`GoogleCalendar`) and syncs its `events.list`; the
calendar list syncs under the per-account `GoogleCalendarList`. It advertises calendar
read/sync **and** writes guarded by `If-Match` (`WriteGuard::Enforced`).

- **IANA-native** (`cal_normalize`): event `start`/`end` are `{ dateTime, timeZone }` with
  an IANA zone (the wall clock is the RFC 3339 value stripped of its offset, paired with
  the `timeZone`) — **no Windows-zone table**. An all-day event is `{ date }`. `location`
  and `description` are plain strings; `hangoutLink`/`conferenceData` video entry point →
  a virtual location; the raw event rides `Event::extended` under `"google/event"`.
- **RRULE strings → the shared parser.** `recurrence` is an array of RFC 5545 `RRULE`
  strings, parsed through `engine_core::calendar::parse_rrule` — the one shared
  RRULE-string parser (CalDAV delegates to it too). Google returns recurring events as
  **masters with an `RRULE`** (`singleEvents=false`), the master + rule + local-expansion
  model the engine wants — cleaner than Graph's pre-expanded `calendarView` (no
  data-loss). A per-instance override (`recurringEventId` set) is dropped (deferred —
  `calendar-semantics.md`); a `status:"cancelled"` entry is a tombstone.
- **Sync** (`cal_fetch`): `events.list` with a per-calendar `nextSyncToken`. The window
  (`timeMin`/`timeMax`) is **optional** and applies only to the initial snapshot (a
  `syncToken` request cannot also carry a window). A `410` on a stale `syncToken`
  classifies as `NeedsResync` → snapshot restart (the same mechanism as Gmail's
  history-expiry; here the error classifier maps `410` directly, no special case).
- **Writes** (`cal_write`): `events.insert` (create), `events.patch` (a partial merge —
  the neutral `EventEdit` intent → a partial event JSON, never re-serializing the
  projection; a start/end move that would change the time *form* is refused),
  `events.delete`. All `If-Match`-guarded (`412` → `Conflict`). Delete idempotency:
  Google signals **already-gone as `404` *or* `410`** (both → success); a `412` on a
  still-existing event is a real conflict (surfaced, not swallowed). A *guarded* re-delete
  returns `412` (the deleted event is left cancelled with a new ETag, failing the stale
  `If-Match`), so the live test does not assert re-delete idempotency — the `404`/`410`
  path is proven offline.

## Testing (3-tier, mirroring Graph — `AGENTS.md` offline-mock caveat)

1. **Offline** (always green): normalizers + error mapping against scrubbed captured
   fixtures; a fixture-routing fake drives snapshot/history-delta/tombstone orchestration.
2. **Capturing-server** (offline, no token): the real reqwest transport at a
   request-capturing server asserts the exact `messages.modify`/`send` **request shapes**
   (the fakes serve canned bytes regardless of request, so these assertions are mandatory).
3. **Live** (gated on `GOOGLE_ACCESS_TOKEN`, skipped otherwise, excluded from coverage):
   the real snapshot→delta cycle, source fetch, send round-trip, and every edit verb
   against the test account. See `crates/provider-google/tests/fixtures/README.md` for the
   captured real-behavior findings (the `Message-ID` rewrite, the `/upload/`-only insert,
   the history-window `404`, IANA-native calendar times).

Google occasionally answers a transient `500 backendError`; it classifies as `Retryable`
and live-test cleanup is best-effort. Push is **poll-first** — no Pub/Sub/webhook infra;
when wanted, it slots behind the existing `Watch` seam (Gmail `users.watch`, Calendar
`events.watch`).

## Google People

`GoogleContactProvider` binds independently to owned Connections, Other
Contacts, Workspace directory people, or contact groups. Each source has its own
scope, token lifecycle, source class, and permission degradation. Connection
tokens that expire with `410` restart as a snapshot; optional-source permission
failures do not fail owned contacts. Contact groups normalize to the same
provider-neutral group-card kind as JMAP and vCard.

`contactGroups.list` has pagination but no sync token, so group sync is always a
snapshot. OAuth capture defaults include
`https://www.googleapis.com/auth/contacts`,
`https://www.googleapis.com/auth/contacts.other.readonly`, and
`https://www.googleapis.com/auth/directory.readonly`; the Workspace directory
scope/source may be unavailable for consumer accounts.

Only owned Connections are writable. The source `etag` is retained in
`RevisionTokens`, copied into update payloads, and carried as the transport
precondition; the adapter advertises `WriteGuard::Enforced`. Other Contacts,
directory entries, and groups are read-only. Group mutation and photo upload
remain deferred; photos are authenticated on-demand reads.
