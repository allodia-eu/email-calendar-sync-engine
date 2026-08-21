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
  `messages.get`, fanned out `MAX_CONCURRENT_GETS` at a time), the history delta
  (`history.list`), and the raw-source fetch.
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
  fixed `metadataHeaders` set for a minimal, deterministic payload). Those per-id gets are
  the pass's whole cost — `messages.list` returns bare `{id, threadId}` and Gmail offers no
  companion batch-get — so a page fetches them **concurrently**, `MAX_CONCURRENT_GETS` in
  flight. Gmail answers `429` past 50 concurrent requests per mailbox whatever the quota
  allows; 20 is the widest window measured clean against a live account (30 draws occasional
  throttles, 50 throttles a tenth). The batch endpoint is not used: a batch of n counts as n
  requests, is no faster at equal width (both shapes cost one round trip), costs ~25% more
  bytes for the multipart envelope, and answers `200` while individual members carry their
  own `429` — so it buys nothing and adds a parser. `tests/live_batch_vs_concurrent.rs` is
  the gated probe that keeps that decision honest. The persisted cursor
  is that captured `historyId` (messages arriving mid-snapshot are re-reported by the
  first delta — idempotent). This is a **reconciling** pass (its present set tombstones
  absent rows). A `SyncWindow` floor windows the enumeration to `q: after:<epoch>`.
- **Delta** (cursor `Some`): `history.list?startHistoryId=…` returns
  `messagesAdded`/`labelsAdded`/`labelsRemoved` — whose message objects are **partials**
  (id + labelIds only) — and `messagesDeleted`, which tombstones. This is an **additive**
  pass.

  A partial's `labelIds` is the message's **resulting** label set, and in Gmail that set is
  the whole of a message's mutable state: labels are both its keywords (`UNREAD`, `STARRED`)
  and its filing (`INBOX`, and every folder-like label). So any label change — a mark-read, a
  star, an archive, a send — is already answered by the page and becomes a `MailStateChange`
  costing **no further request**. Only `messagesAdded` is re-fetched full, because nothing in
  a history record carries a subject, a sender or a body. A record whose `labelIds` is
  *absent* is re-fetched too: absent is not empty, and reading it as empty would mark unread
  mail read (`keywords_from_labels` reads the absence of `UNREAD` as `$seen`).

  That is also how a message *becomes sent* here: `drafts.send` adds `SENT` to the message the
  draft already had, so the send arrives as a `labelsAdded` record for a key the store is
  holding and never as a `messagesAdded` — which is why the recipient observations read an
  update's state changes and not only its whole objects (`store-and-sync.md`). Pinned live in
  `live_a_label_change_comes_back_as_state_not_a_whole_message`.

  A `404` (the
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
  **A `MoveTo` to the synthetic `ALL_MAIL` is the archive, and adds no label at all**:
  Gmail has no Archive place — archiving *is* the absence of `INBOX` — and `ALL_MAIL` is an
  id this adapter reserves for the mailbox it synthesizes (`normalize::ALL_MAIL_ID`), so
  sending it back as a label would be a `400` on a name Gmail has never heard of. The
  removals alone do the work. Nothing offline can catch getting this wrong (the fakes answer
  canned bytes regardless of the request), so it is pinned by a request-body assertion
  (`mutate_tests`) **and** a live round-trip (`live_archive_to_all_mail_…`).
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
- **Both scheduling capabilities are constants here** (issue #105).
  `Capabilities::calendar_scheduling` is `true` — the service sends the iTIP
  `REQUEST`/`REPLY`/`CANCEL` a write implies, with no opt-out a client can reach (the
  notify controls above choose *whom* it tells, not whether it is the one telling them), so
  unlike CalDAV there is nothing to discover. `Capabilities::scheduling_submission` is
  **also** `true`, from the mail side: this adapter submits assembled RFC 5322 bytes through
  `engine-rfc5322`, so it owns every `Content-Type` parameter including the `method=` that
  makes an iTIP object a scheduling message. It is therefore usable as the *sending*
  transport for an account whose **calendar** lives on a plain CalDAV server (`providers.md`).
- **RSVP** (`cal_write::rsvp_event`): a one-element `attendees` array carrying the matched
  address' `responseStatus` (+ `comment`), guarded by `EventRsvp::guard` (the intent's
  revision, **not** `base`'s), with **`sendUpdates` as a query parameter** — in the body it
  is silently ignored and the organizer is simply never told. Live findings
  (`tests/live_calendar_rsvp.rs`):
  - **As an attendee, the one-element array does not truncate** the guest list: Google
    applies only the caller's own status and leaves the other attendees alone. **As the
    organizer it replaces the array** and the other guests are dropped — the known gap, now
    pinned by a test rather than assumed. A host should answer as an attendee.
  - **`participants` merges the organizer with their `attendees[]` entry** (one participant
    per address, roles as a set, the attendee entry's status winning). Google names the
    organizer twice, and the pre-merge projection reported both — a self-organized event
    read back as `accepted` no matter what had been answered. See `calendar-semantics.md`.
  - `events.list` is **read-your-writes** for an answered `responseStatus` (an immediate
    re-list shows it; no poll needed), unlike People's sync tokens.
  - Answering an address the event does **not** yet carry is accepted, not rejected: Google
    creates the attendee at that status. That is how a self-organized event gets its own
    attendee entry at all.
  - The live suite seeds a *genuine* invitation with **`events.import`** — an event whose
    `organizer` the account is not, and which preserves the caller's `iCalUID` (unlike
    `events.insert`, which mints its own). Nothing is mailed. That is the only way to get a
    real `needsAction` invitation on a single test account.

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

**People is not served from `www.googleapis.com`.** Gmail (`gmail/v1/…`) and
Calendar (`calendar/v3/…`) share the universal API host, but People does not:
both `www.googleapis.com/v1/people/…` and the service-prefixed
`www.googleapis.com/people/v1/…` answer an **HTML `404`**. Contact paths are
therefore rooted at `https://people.googleapis.com` via
`GoogleClient::people_url`, while `GoogleClient::url` keeps mail and calendar on
the universal host. A client built with a custom base (replay server, proxy)
still wins for both, so offline tests are unaffected — which is precisely why
this was invisible offline: the fixture fakes and the replay server set a custom
base, so every contact URL resolved to the test origin regardless of host.
Verifying a People request shape requires the gated live test.

The People API must also be **enabled per Cloud project**; until it is, every
call answers `403 SERVICE_DISABLED` from `people.googleapis.com` (a JSON
envelope, distinct from the HTML 404 above — the two are worth telling apart when
diagnosing).

`GoogleContactProvider` binds independently to owned Connections, Other
Contacts, Workspace directory people, or contact groups. Each source has its own
scope, token lifecycle, source class, and permission degradation. Connection
tokens that expire with `410` restart as a snapshot; optional-source permission
failures do not fail owned contacts. Contact groups normalize to the same
provider-neutral group-card kind as JMAP and vCard.

**A People page with nothing to report omits its collection key entirely** — a quiet
incremental sync answers exactly `{"nextSyncToken": "…"}`. An absent collection is
read as "no entries" only when the page still carries a cursor; a page with neither
is malformed and must not advance a cursor, so a token-less source
(`contactGroups`) stays strict and a bad page can never empty the store.

Each source's field mask is its own. `otherContacts.list` accepts **only**
`names`, `emailAddresses`, `phoneNumbers`, `photos`, `metadata`; any other field
fails the whole request with `400 INVALID_ARGUMENT`. Optional sources degrade to
`Unavailable` on `403` or `400 FAILED_PRECONDITION` (a consumer account has no
Workspace directory) but **never** on `400 INVALID_ARGUMENT` — masking that would
turn a wrong request into a silently empty address book. A stale-etag write is
also `400 FAILED_PRECONDITION`, classified `Conflict`, not `412`.

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

`PropertyId`s are `{field}-{index}`, **never** `metadata.source.id`. That field
identifies the source *record* — every email, phone, address, and organization
of one person carries the same value — so keying a `BTreeMap` on it collapsed
each multi-valued field to its last entry. An offline fixture that omits
`metadata.source` cannot catch this; the fixtures now carry the shape a real
account returns.

Continuation tokens (`syncToken`, `pageToken`, `startHistoryId`) are opaque
server strings and go through `transport::encode_query_value` before being
spliced into a query, in mail and calendar as well as contacts. Unencoded, a
token containing `&` or `=` re-parameterizes the request and the client fetches
a page the server never named.

## Contact photos and their size

`photos[].url` is served from `googleusercontent.com`, a different origin from the
API, so the fetch is unauthenticated (`providers.md` → "Credentials and remote-content
URLs"). A person with no picture has only a `default: true` entry — Google's generated
monogram — which the normalizer drops, so a card either advertises a real photo or
advertises none.

**The size is a path suffix, `…=s240`, and `?sz=` is a silent no-op.** The CDN accepts
the query parameter and answers `200` with the *original* bytes, so a caller using it
gets a plausible image and no signal that nothing happened. Measured against a real
contact photo:

| URL | Result |
|---|---|
| bare | 512x512, 65 KB |
| `?sz=240` | 512x512, 65 KB — ignored |
| `=s240` | 240x240, 17 KB |

`photos[].url` arrives already carrying a suffix (`=s100`, sometimes with flags like
`-c`), so the suffix is *replaced*, not appended. Nothing offline can distinguish a
working size request from an ignored one — both are a successful fetch of a valid
image — so `live_a_contact_photo_arrives_at_the_size_we_asked_for` asserts the returned
image's **pixel dimensions**. The card keeps the original URL, so the cache key is
unaffected.

A person with no picture still gets a `photos[]` entry — flagged `"default": true`, serving
Google's own generated monogram. The normalizer drops those, so a card either advertises a
real photo or advertises none; publishing one would put a *Google* avatar next to a sender
in place of the host's own. Pinned offline against `contacts/connections_photos.json`,
whose URLs keep their real `=s100` suffix so the replacement path is the one exercised.

## Reporting a message (junk / not junk)

`users.messages.modify`, in `crate::report`. Gmail has no report endpoint: its filter
learns from the `SPAM` label, so **the label is the report**, not a move that accompanies
one. Three behaviours, all verified live and none of them documented:

- **Adding `SPAM` files the message by itself.** The server drops `INBOX` without being
  asked (`["UNREAD","SENT","INBOX"]` → `["UNREAD","SENT","SPAM"]`), so there is no separate
  move to make and no way to report without moving.
- **Removing `SPAM` does *not* put the message back.** It leaves it in no place label at
  all — archived, and gone from the folder the user was looking at. The not-junk direction
  therefore adds the destination explicitly; that is the one reason
  `MessageReport::destination` is read here rather than ignored as it is on Graph.
- **There is no phishing verdict.** The system label set has no member for it and
  `messages.modify` answers `400 Invalid label: PHISHING`. The adapter advertises
  `ReportVerdicts::without_phishing`, and `label_delta` refuses the verdict *locally* as
  well — a guard that lives only in the capability check upstream is one refactor from
  being skipped.

A junk report leaves the message where the engine can still see it, but only because the
snapshot asks for it — see "Spam and Trash are not optional" below. Without that flag the
not-junk direction would be unreachable from a synced row: the message the user has to
press "not junk" on would not be in the account the engine shows.

`keyword_label_delta` is the other half of this. A keyword Gmail has no label for is an
error, not a silent drop — a `$junk` write that reported success and did nothing is exactly
the shape this mapping invites — and for the three junk keywords the error names
`report_message` as the way to say it.

## Spam and Trash are not optional in the snapshot

`messages.list` omits `SPAM` and `TRASH` unless `includeSpamTrash=true`. `history.list`
takes no such flag and reports their label changes regardless. The two passes therefore
disagree, and not symmetrically: **a snapshot tombstones every key absent from its present
set**, so the snapshot wins by deleting what the delta had just filed correctly. Which one
the store believes comes down to whether the last pass happened to be a snapshot, and a
`historyId` aging out is enough to turn one into the other.

So `fetch::list_url` always sets the flag — including on the windowed `q=after:<epoch>`
shape, which excludes them just as the unwindowed one does (verified live; an explicit
`q=in:trash` does *not*, which is why reading the flag's docs is misleading here).

Two consequences worth knowing before touching this:

- **A snapshot fetches every spam and trash message full**, one `messages.get` each, the
  same as any other. Gmail purges both after 30 days and the sync-window floor bounds it
  further, but an account with a large Trash pays for it on every reconcile.
- **Spam and Trash are ordinary place labels here**, so `memberships_of` files a message
  in them like any other folder and the roles are already mapped (`SPAM` → `Junk`,
  `TRASH` → `Trash`). A message with only keyword labels still falls through to the
  synthetic All Mail, which is why spam does not appear there — matching Gmail's own
  All Mail, which excludes both.

`live_spam_trash.rs` drives both passes over one message and asserts they agree; it fails
on the snapshot half alone if the flag is dropped.
