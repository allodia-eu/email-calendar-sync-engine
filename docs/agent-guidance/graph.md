# Microsoft Graph Client Guidance

This document is authoritative for the **Microsoft Graph provider client**
(`provider-graph`) — the first external cloud-mail adapter. Read it before
touching `provider-graph` or the Graph mail sync path, alongside `providers.md`
(the Provider Contract), `store-and-sync.md` (the apply/lease model), and
`modeling.md`.

Graph is the cloud-API counterpart to JMAP (OAuth bearer + JSON over HTTP), but
its mail **sync shape is IMAP/CalDAV-like, not JMAP-like**: there is no
account-wide message delta, so sync is per folder.

## The crate

`provider-graph` implements the `engine_provider::Provider` contract for **mail
(read/sync + submission + writes)** and **calendar (read/sync + writes)** — mail on
`GraphProvider` (folder-bound), calendar on `GraphCalendarProvider` (calendar-bound),
each over its own `GraphClient` on the same token. The mail layers:

- **`error`** — `GraphError` (`Status`/`Json`/`Protocol`/`Transport`) → the
  engine-neutral `FailureClass`. Graph error bodies are a documented
  `{ "error": { "code", "message" } }` envelope; the `code` is captured for
  diagnostics, the HTTP status drives classification (`401`→auth, `429`→rate
  limit, `410 Gone`→`NeedsResync` for an expired delta token, `5xx`→retryable).
- **`json`/`normalize`** — pure `serde_json::Value` → `Mailbox`/`Message`,
  unit-tested against captured fixtures.
- **`transport`** — a `GraphTransport` seam over bearer HTTP. `HttpTransport`
  (reqwest + rustls, built from the caller-supplied `TlsClientConfig` passed to
  `GraphClient::connect`/`for_mailbox`/`with_base` — `tls.md`) is production; the
  seam lets the fetch/provider orchestration run offline against fixtures. There is **no session discovery**
  (the v1.0 root is fixed); requests carry `Prefer: IdType="ImmutableId"`.
  Having no connect-time request, Graph is the one adapter whose
  `ConnectionInfo::http_version` is `None` until its first fetch — the transport
  records it at its single `send` funnel (`providers.md`, `tls.md`). For the same
  reason it takes **no** `ConnectObserver` and emits **no** `ConnectStep`: there is no
  connect exchange to observe, and `GraphClient::connect` performs no I/O at all. That
  absence is documented, never faked with a synthetic step.
  `GraphClient::with_base` overrides the API origin (a forward proxy, a regional/
  sovereign endpoint, or the test replay server), **rebasing** the absolute
  `@odata.nextLink`/`deltaLink` URLs Graph returns onto that origin so
  link-following stays on the chosen endpoint.
- **`fetch`** — folder-list resolution and the message snapshot/delta + re-fetch
  paging.
- **`submit`** — mail submission via `POST /me/sendMail` in **MIME format** (see
  **Submission** below). Adds a `post` verb to the `GraphTransport` seam.
- **`mutate`** — mutating mail writes (`edit_mail`: mark-read/flag, move, delete) via
  the `patch`/`post` verbs (see **Mail writes** below). Account-level like submission,
  keyed by the message's immutable id, so any folder-bound provider can edit any
  message.
- **`provider`** — `GraphProvider`, bound to one folder for email; submission and
  writes are account-level, so every bound provider advertises them.

## Graph specifics implemented

- **Per-folder mail delta.** `/me/messages/delta` returns `400` — there is no
  account-wide message delta. Message delta is rooted at a folder
  (`/me/mailFolders/{id}/messages/delta`) with a per-folder `@odata.deltaLink`
  cursor. So a `GraphProvider` is **bound to one folder** (its `email_scope` is
  `SyncScope::GraphFolder`), the folder list syncs under the per-account
  `SyncScope::GraphFolderList`, and the **cross-folder fan-out is the
  orchestrator's job** — the same shape as `provider-imap`.
- **Immutable ids are the `ProviderKey`.** `Prefer: IdType="ImmutableId"` yields
  ids that are stable across folder moves and URL-safe (Graph's default ids
  change on move). A message's single-folder membership comes from
  `parentFolderId` (Graph mail is one-folder, like an IMAP copy — not the
  multi-membership JMAP/Gmail shape).
- **Roles resolved by id, never by name.** A personal `mailFolder` carries **no**
  `wellKnownName` (selecting it `400`s) and a **localized** `displayName`
  (e.g. Dutch "Postvak IN"). The provider `GET`s the well-known aliases
  (`inbox`, `archive`, `drafts`, `sentitems`, `deleteditems`, `junkemail`) to
  learn their ids and matches by id; `msgfolderroot` is resolved to null the
  parent of top-level folders. `outbox`/conversation-history have no standard
  role.
- **Snapshot = the initial delta enumeration** (full objects): drain
  `@odata.nextLink` pages, ending at the `@odata.deltaLink` that becomes the
  persisted cursor. `$top` does **not** paginate consumer delta (page size is
  server-controlled; `@odata.nextLink` appears only on large result sets).
- **Incremental delta: full objects, except lightweight changes.** Microsoft's
  [delta-query-messages](https://learn.microsoft.com/graph/delta-query-messages)
  guidance says a changed entry is a *full* object — and it is for substantive
  edits (verified live: a flag change returns every selected field + `@odata.etag`).
  The exception, **not in the docs** and observed on consumer mailboxes, is a
  *lightweight* property change (notably `isRead`): it returns only the changed
  property + `id`, with **no** `@odata.etag`. So the adapter uses the entry
  directly when it carries `@odata.etag` (the common case) and resolves the
  etag-less partials as **state changes** through the narrow `MESSAGE_STATE_SELECT`
  rather than re-fetching the whole message.

  That narrow read cannot ask for the etag: `@odata.etag` is an OData *annotation*,
  not a property, so naming it in a `$select` is an error — whether a `$select`ed
  single-entity `GET` answers with one is the service's choice. It does
  (live-verified, captured as `tests/fixtures/mail/message_state.json`, where
  `@odata.etag` is `W/"{changeKey}"` verbatim), and both the offline test and
  `live_an_is_read_change_comes_back_as_state_not_a_whole_message` assert it, because
  the etag is the token an `If-Match` quotes and nothing in the request asks for it.
  If it ever stops, the store keeps the stored token rather than blanking it
  (`RevisionTokens::or`, `store-and-sync.md`), so the failure degrades to a stale
  guard instead of to none. A
  removal is `{ id, @removed: { reason } }` → an inline tombstone. (JMAP differs
  again: `Foo/changes`→`Foo/get` always yields full objects.) The rest of the flow
  follows the doc verbatim: initial `messages/delta` with `$select`, drain
  `@odata.nextLink` to the terminal `@odata.deltaLink` (the persisted cursor),
  following the returned URLs as-is since the token encodes the `$select`.
- **Streaming + sync-depth window.** `stream_email` streams the folder's mail as
  `EmailChunk`s: each `messages/delta` page is fetched whole over HTTP and re-chunked
  with `split_page` — a first-sync snapshot into `Reconcile` chunks (whose accumulated
  `present` set tombstones absent rows at end of pass), a delta into `Additive` chunks —
  and a final marker chunk carries the `@odata.deltaLink` cursor. A consumer delta is
  not cheaply resumable mid-pass, so intermediate chunks **hold** the cursor and a crash
  re-runs the pass. A `SyncWindow { since }` passed **per sync** bounds the initial
  snapshot via a `receivedDateTime ge` `$filter`, so a large folder syncs only recent
  mail. A delta cannot carry a filter (the `deltaLink` is opaque), so a message moved into
  the folder is reported however old it is; the engine drops it on apply
  (`SyncWindow::admits`, `store-and-sync.md`), which is where the bound holds for every
  adapter. `GraphProvider::with_since` survives only as the `default_sync_window` the
  whole-scope `sync_email` drain fetches under. The `chunk_size` knob is the commit
  granularity; the `fetch_batch` knob has no lever yet (page size is server-controlled —
  see Known limitations).
- **Keyword/revision mapping.** `isRead`→`$seen`, `isDraft`→`$draft`,
  `flag.flagStatus == "flagged"`→`$flagged`; `internetMessageId` is preserved
  bracket-stripped as a threading hint (never identity); `conversationId`→thread
  provenance; `@odata.etag`→`ETag` and (full `GET` only) `changeKey`→`ChangeKey`
  revision tokens; `bodyPreview`→the snippet.

## Submission (mail send)

`submit_email` sends via `POST /me/sendMail` in **MIME format** (`Content-Type:
text/plain`, the RFC 5322 message base64-encoded as the body), *not* the JSON
`message` resource. The reason is the Write Contract (`store-and-sync.md`): the JSON
form lets Graph mint its own `Message-ID`, which breaks reconcile-by-`Message-ID`
when the sent copy syncs back and cannot carry `In-Reply-To`/`References` (Graph's
`internetMessageHeaders` only accepts custom `x-*` headers). The MIME form ships the
whole message the caller assembled — pre-generated `Message-ID`, threading, `Cc`/
`Bcc`, an HTML alternative, attachments — verbatim, exactly like IMAP's SMTP `DATA`.

- **One shared assembler.** The RFC 5322 / MIME bytes come from **`engine-rfc5322`**
  (`assemble_filed_message`) — the same crate `provider-imap` feeds to SMTP `DATA`,
  hoisted out of `provider-imap` so both adapters share one hardened, tested
  assembler rather than duplicating RFC 5322 correctness. The **filed** variant is
  used (it keeps the `Bcc` header): Graph reads every recipient from the MIME to build
  the delivery envelope and strips `Bcc` before delivering, so the Sent-Items copy
  records whom the sender Bcc'd while no recipient sees it. Graph files the Sent copy
  itself; there is no separate `APPEND` step (unlike IMAP).
- **No returned id, like SMTP.** `sendMail` answers `202 Accepted` with **no body**,
  so there is no server key for the sent copy. The `SubmissionReceipt` carries a
  `Message-ID`-derived placeholder key (`sent:<Message-ID>`, mirroring IMAP's
  no-`UIDPLUS` filing key) and echoes the `Message-ID`; the real sent object
  reconciles by `Message-ID` when Sent Items next syncs. A malformed MIME body is the
  documented `400 ErrorMimeContentInvalidBase64String` (permanent); `401`/`429`/`5xx`
  classify as auth/rate-limit/retryable through the shared status mapping.
- **Live-verified.** A self-addressed send against the real account is confirmed to
  come back into the Inbox carrying the *exact* pre-generated `Message-ID` — proving
  Graph preserves it in the MIME form (`tests/live_provider.rs`, gated on
  `GRAPH_ACCESS_TOKEN`). The `Mail.Send` delegated scope is required.

## Mail writes (mark-read/flag, move, delete)

`edit_mail` applies a neutral [`MailEdit`] to an already-synced message, keyed by its
immutable id — so, like the IMAP adapter, one folder-bound provider can edit a message
in **any** of the account's folders (the target's mailbox comes from its key, not the
bound folder). Graph advertises `mail_writes`. The three neutral edits map onto three
different Graph shapes (Graph models mail state as typed properties, not a keyword set):

- **`SetKeywords` → `PATCH /messages/{id}` `{isRead, flag}`.** `$seen`→`isRead` (bool),
  `$flagged`→`flag.flagStatus` (`flagged`/`notFlagged`). These are the **only** two
  writable keyword-like properties Graph exposes, so any other keyword is **rejected**
  (`InvalidState`), never silently dropped — `$draft` is read-only and Graph categories
  are a separate concept. Both sides empty is a no-op (no request). The edits are
  **unconditional** (no `If-Match`): the `MailEdit` shape carries no ETag guard, like
  IMAP `UID STORE` and JMAP `Email/set`.
- **`MoveTo` → `POST /messages/{id}/move` `{destinationId}`.** Immutable ids are stable
  across a move (live-verified: the `201` echo keeps the id, updates `parentFolderId`),
  so the receipt key is the **unchanged** target and the destination folder reconciles
  the new membership on its next sync — the JMAP shape, not IMAP's synthesize-a-new-key.

  A consequence worth knowing before writing a read: because the id survives and mail sync
  is **per folder**, one provider key is legitimately held by **two** scopes between the
  destination folder's delta creating it and the source folder's delta reporting it
  `@removed`. Nothing in the store forbids it — its key is `(scope, key)` — so an
  account-wide read must not assume a key resolves to one row. `Engine::compose` composes
  such a key once, keeping the row with the later `last_modified`; see `engine-api.md`.
- **`Delete` → `POST /messages/{id}/permanentDelete`.** The neutral `Delete` is a
  **permanent**, irreversible delete (a Trash *move* is `MoveTo(trash)`), so it uses
  `permanentDelete`, not `DELETE /messages/{id}` (which only soft-deletes to Deleted
  Items). The bodyless `POST` **must** send `Content-Length: 0` or Graph returns `411
  Length Required` (reqwest omits the header for an empty body, so the transport sets it).
  An already-gone message (`404`) is idempotent success; the ambiguous re-delete Graph
  answers with `403 ErrorCannotDeleteObject` (the purged item lingers, still `GET`-able by
  id during retention) **propagates**, left to the outbox's `NeedsConfirmation` — the same
  shape as the calendar delete.

**Live-verified.** `tests/live_provider.rs` sends a self-addressed message, then
mark-reads + flags it (asserting the re-sync reflects both keywords), moves it to Archive
(asserting it leaves the inbox), and permanent-deletes it — all against the real account.
The `Mail.ReadWrite` delegated scope is required.

## Shared mailboxes (the multi-mailbox model)

One signed-in user (one OAuth credential) can access several mailboxes: their own
and any shared/other mailbox they hold delegate access to — Graph addresses the
latter as `…/users/{address}/mailFolders('Inbox')/messages`, using the user's
token plus the `*.Shared` delegated scopes (an Exchange Online / work-school
feature; the `tools/graph-oauth` helper already requests them).

The engine models this **without any `engine-core` change**, because it is already
multi-account:

- **Each mailbox is a separate `AccountId`** — its own folders, `GraphFolder`/
  `GraphFolderList` scopes, cursors, search, and threading, exactly like any other
  account. A shared mailbox reuses the entire existing machinery; nothing about it
  is special at the store/sync/search layer.
- **The credential is shared.** Credentials live outside the store (host-owned —
  `north-star.md`), so several accounts can map to the same token. The host's
  account onboarding owns the credential → accounts mapping and the
  add-a-shared-mailbox flow (deferred).
- **The provider differs only by a `MailboxPrincipal`.** `GraphClient::for_mailbox`
  roots every request at `/me` (`MailboxPrincipal::Me`) or `/users/{address}`
  (`MailboxPrincipal::user`); the rest of the provider — folder list, role
  resolution, snapshot/delta, re-fetch — is principal-agnostic. This stays in
  `provider-graph`: a Graph-specific URL detail does **not** belong in generic
  `engine-core` types (AGENTS hard rule).
- **Unified "all my mailboxes" views are host-composed**, not storage joins
  (`north-star.md`). Search/threading remain per-account.

So adding a shared mailbox is, for the engine, just another account pointed at a
`User` principal. (Not live-verified — a personal Microsoft account cannot host
shared mailboxes; verification awaits a work/school account.)

## Known limitations (documented, not bugs)

- **Tier-1 metadata only.** The body/MIME and Graph `uniqueBody` are fetched on
  demand in a later store sub-step, not materialized here.
- **No cross-folder orchestration yet.** The provider is folder-bound; syncing
  every folder is the orchestrator's job (the live test binds the inbox alias).
- **Top-level folders only.** `GET /me/mailFolders` lists the children of
  `msgfolderroot`; a folder nested under another folder is not yet discovered (a
  `childFolders` traversal is a follow-up). `folder_from_json` already preserves a
  non-root parent for when nested discovery lands. The list *is* fully paginated
  (`@odata.nextLink` drained), and a well-known role alias that 404s (unprovisioned
  on the account) is skipped rather than failing the whole folder list.
- **Per-id delta re-fetch (and role resolution) are sequential GETs.** A changed id
  is re-fetched with one `GET` each, and the 6 role aliases + `msgfolderroot` are
  resolved with one `GET` each per folder-list pass; both could collapse to a few
  round-trips via `$batch` (≤20 sub-requests) — a follow-up optimization. A
  changed-id re-fetch that `404`s (deleted/moved in the race since the delta) is
  skipped, so a single vanished message cannot wedge the pass.
- **Page size is server-controlled.** The delta cycle drains every server page
  (correct), but the adapter does not yet send `Prefer: odata.maxpagesize` — the
  page-size control the delta-query-messages doc documents — so it ignores the
  `stream_email` `fetch_batch`. A follow-up for responsive streaming. (`$top` does
  *not* paginate consumer delta, which is why the header is the right lever.)
- **National clouds aren't auto-rebased.** `with_base` rebasing rewrites only the
  commercial-cloud origin (`graph.microsoft.com/v1.0`); links a national-cloud
  endpoint (e.g. `graph.microsoft.us`) returns would be followed verbatim — fine
  for the replay server and a same-origin proxy, a gap for true national clouds.
- **Snapshot order is delta-defined**, not newest-first (consumer delta has no
  `$orderby`). A streaming newest-first snapshot via the list endpoint is a
  possible later refinement.
- **Mail read/sync + submission + writes (`GraphProvider`).** The provider advertises
  `mail` + `mail_writes` + `submission` (see **Submission** and **Mail writes** above).
  Both submission and writes are account-level (keyed by immutable id), so they need no
  per-folder config. Calendar is a separate provider (see **Calendar** below), not this one.

## Calendar (implemented)

`GraphCalendarProvider` implements the calendar read/sync **and** write spine, bound to
one calendar (`SyncScope::GraphCalendar`), with the calendar list under the per-account
`SyncScope::GraphCalendarList` — the same shape as the mail folder/folder-list split.
Layers: `cal_fetch` (calendar list + `calendarView/delta` paging), `cal_normalize`
(event/calendar JSON → model) + `cal_recur` (`patternedRecurrence` → `Recurrence`),
`windows_zones` (Windows→IANA), `cal_write` (create/patch/delete), `calendar` (the
provider). It advertises `calendars` **and** `calendar_writes(WriteGuard::Enforced)`.

- **`calendarView/delta` is the source, masters + local expansion.** Event delta is
  `GET /me/calendars/{id}/calendarView/delta?startDateTime=…&endDateTime=…` (v1.0's only
  windowed delta; `/me/events` has no v1.0 delta). It returns the series `seriesMaster`
  (with `patternedRecurrence`), standalone `singleInstance`s, the server's pre-expanded
  `occurrence`s, and per-instance `exception`s, ending at an `@odata.deltaLink`. The
  engine stores a master + rule and expands **locally**, so the adapter projects only
  `seriesMaster`/`singleInstance` and **drops `occurrence`** (re-expanded from the master)
  **and `exception`** (see the limitation below). This reuses the mail delta machinery
  (`@odata.nextLink`/`deltaLink`/`@removed`, `410`→snapshot restart).
- **Time-windowed, per calendar.** The mandatory date range comes from a host-supplied
  `CalendarWindow` (its recurrence-expansion horizon; `providers.md`); the `deltaLink`
  encodes it, so it is applied only to the initial request.
- **Authoring-zone times via `Prefer: outlook.timezone`.** A plain read returns UTC,
  which expands a recurring master DST-incorrectly. The provider sends `Prefer:
  outlook.timezone="<display_zone IANA>"` (the host's home/display zone) on every
  `calendarView` request, so Graph returns each event's wall clock in that zone (echoing
  the IANA name), which the adapter stores. Windows zone names (a read without the header)
  still map through the CLDR `windowsZones` table (`windows_zones`, CLDR 49); an unknown
  or `tzone://Microsoft/Custom` zone is preserved as a custom zone, never guessed.
- **Recurrence mapping.** `patternedRecurrence` `pattern`+`range` → one `RecurrenceRule`:
  `daily`/`weekly`/`absoluteMonthly`(BYMONTHDAY)/`relativeMonthly`(BYDAY+`index`)/
  `absoluteYearly`/`relativeYearly` → `FREQ`+`BY*`; `range` `noEnd`/`numbered`/`endDate`
  → unbounded/`COUNT`/`UNTIL`. Graph's full weekday names map to the engine `Weekday`.
- **Writes — the server does the surgery (like JMAP).** `create_event` `POST`s to
  `/me/calendars/{id}/events` (Graph assigns id **and** `iCalUId` — a client `UID` is not
  accepted, so the receipt carries the server's id/uid); `patch_event` translates the
  neutral `EventEdit` intent into a **partial** event `PATCH` (never re-serializing the
  projection); `delete_event` `DELETE`s. All are `If-Match`-ETag guarded — a stale one is
  a `412` → `Conflict` — so Graph advertises `WriteGuard::Enforced` (unlike JMAP). A start
  move is rejected if it would change the time *form* (`has_same_form`). The raw Graph
  event JSON is preserved beside the projection in `Event::extended`
  (`"microsoft.graph/event"`), since Graph is neither iCal nor JSCalendar.
- **Both scheduling capabilities are constants here** (issue #105).
  `Capabilities::calendar_scheduling` is `true` — the service sends the iTIP
  `REQUEST`/`REPLY`/`CANCEL` a write implies, with no opt-out a client can reach (the
  notify controls above choose *whom* it tells, not whether it is the one telling them), so
  unlike CalDAV there is nothing to discover. `Capabilities::scheduling_submission` is
  **also** `true`, from the mail side: this adapter submits assembled RFC 5322 bytes through
  `engine-rfc5322`, so it owns every `Content-Type` parameter including the `method=` that
  makes an iTIP object a scheduling message. It is therefore usable as the *sending*
  transport for an account whose **calendar** lives on a plain CalDAV server (`providers.md`).
- **RSVP** (`cal_write::rsvp_event`): `POST /me/events/{id}/accept|tentativelyAccept|decline`
  with `{comment, sendResponse}`. Proven against two real accounts
  (`tests/live_calendar_rsvp.rs` — the only test in this repo that needs a second mailbox,
  because Graph cannot fake an invitation: an event created in a mailbox always has that
  mailbox as organizer, and a mailbox cannot answer its own meeting). Live findings:
  - **`sendResponse: true` really schedules the reply** — the *organizer's* copy shows
    `tentativelyAccepted` within seconds. Unobservable from the answering mailbox, whose own
    copy changes either way, which is why the test reads the counterparty's mailbox.
  - **An invitee's copy lists the organizer twice** — as `organizer` *and* as an
    `attendees[]` entry — while the organizer's own copy omits them from `attendees`.
    `cal_normalize::participants` merges the pair into one participant (roles unioned), and
    deliberately does **not** take that entry's status: Graph writes `"none"` there because
    it never records a response from an organizer, so adopting it would report the person who
    called the meeting as not having answered. `"none"` on a real guest still means
    "has not responded". Google is the mirror image (it *does* track the organizer's own
    status) — `calendar-semantics.md`.
  - **`WriteGuard::Absent` is observed, not cautious.** The action endpoint has no working
    precondition: a matching `If-Match` is accepted and ignored (`202`), and a malformed one
    is a `500 ErrorInternalServerError` rather than a `412`. So `rsvp.guard` cannot be sent,
    answering from a stale read is not refused, and the live test asserts that.
  - **Declining removes the event from the invitee's calendar** (Outlook's default), so a
    declined event cannot be re-read or re-answered — a subsequent `GET` is
    `404 ErrorItemNotFound`. Any test answering more than once must decline last.

### Calendar limitations (documented, not bugs)

- **Per-instance overrides are not reconciled.** A moved/cancelled single occurrence is a
  Graph `exception` object, but Graph v1.0 exposes **no `recurrenceId`/`originalStart`** on
  it (both `null`, even on a direct `GET`), so the override cannot be keyed onto the
  series — and the engine's cross-object master/override dedup is itself staged
  (`calendar-semantics.md`). Exceptions are therefore **dropped**: the series expands by
  its rule, and a per-occurrence edit is not yet reflected. (Also why `patch_event` supports
  only `PatchTarget::Series`; a per-occurrence patch returns `InvalidState`.)
- **One display zone per provider.** All events are read in the provider's `display_zone`
  (Outlook's own behavior), so a recurring event *authored* in another zone expands in the
  display zone — correct to the instant, but its DST transitions follow the display zone.
- **Windowed coverage.** Sync covers the `CalendarWindow` only; events outside it are not
  synced (the `providers.md` "possibly-incomplete coverage" model). Coverage reporting is
  a follow-up.
- **No alerts / `SEQUENCE`.** Graph reminders → engine `Alert`s, and iTIP `SEQUENCE`, are
  not yet mapped (the reminder rides the preserved raw). `create_event` rejects a
  floating-time start (Graph has no floating events).

## Testing

- **Offline (always green, no network):** the normalizers and error mapping are
  driven by **scrubbed real Graph responses** captured from a throwaway account
  (`tests/fixtures/`, with a `README.md` recording provenance + the scrub). A
  fixture-routing fake transport (`test_support`) exercises the folder/snapshot/
  delta/re-fetch/tombstone/pagination orchestration; a blocking mock HTTP server
  exercises the real reqwest transport and the status/transport classification.
  For the calendar slice, the same fake transport drives the calendar-list /
  `calendarView`-delta orchestration (masters/singles kept, occurrences/exceptions
  dropped, `@removed` tombstones), the `patternedRecurrence`→`Recurrence` and Windows/
  IANA-zone normalizers, and the create/patch/delete body shapes + form guard + `409`/
  `412` conflict mapping. The MIME `sendMail` request shape is asserted by a **capturing**
  mock server that decodes the posted base64 (`test_support::capturing_server`); the same
  capturing server asserts the mail-write shapes — the `PATCH {isRead,flag}` body, the
  `POST …/move {destinationId}` body, and the `POST …/permanentDelete` verb with its
  `Content-Length: 0` — since the fixture-routing fake ignores request bodies.
- **End-to-end replay (deterministic, runs in CI, no token):** a fixture-replay
  HTTP server (`test_support::replay_server`) serves the captured responses over
  real HTTP, and `GraphClient::with_base` points the real client at it. Tests drive the
  **whole stack** — reqwest transport + `@odata`-link rebasing + orchestration — without
  a token, for both the mail folder/snapshot/delta path and the calendar event snapshot.
- **Live (gated on `GRAPH_ACCESS_TOKEN`, skips otherwise):**
  `tests/live_provider.rs` checks folder role resolution, the snapshot→delta
  cycle, a **self-addressed `sendMail`** whose exact pre-generated `Message-ID`
  is polled back out of the Inbox (proving Graph preserves it in the MIME form), and a
  **calendar cycle** — list calendars, snapshot+delta the events, then create→patch
  (asserting the ETag advances)→delete a throwaway event — all against a *real* account,
  an occasional drift check against the actual API, not the CI gate. There is no CI harness (no live account in CI); the token
  is obtained with `tools/graph-oauth` (a standalone PKCE-loopback login + refresh
  helper, outside the engine workspace). Excluded from the offline coverage metric
  via the `ci.yml` `--ignore-filename-regex`, like the other providers' live tests.

## Contacts

`GraphContactProvider` is source-bound independently from the mail/calendar
providers. Personal root/folder contacts use per-collection delta links and are
writable; contact folders are discovered recursively. Organizational contacts
and directory users use global Graph endpoints and degrade to source-level
`Unavailable` on missing optional permission, so personal contacts still sync.
Every normalized card preserves raw JSON and `changeKey`.

**The tenant sources do not answer `403` on a personal account.** A personal MSA
refuses them by *shape*, not by permission: `/contacts/delta` answers
`400 BadRequest` ("This API is not supported for MSA accounts") and `/users/delta`
answers `401` with an empty message. The degradation rule therefore treats
`400`/`401`/`403` as "source unavailable" **for the optional sources only**.
Swallowing `401` there cannot hide an expired token, because the same credential
drives the personal source, which is never optional and still surfaces the
failure. Captured in `tests/fixtures/error/contacts_*.json`.

Two normalization rules the captured fixtures pin, both invisible to hand-written
JSON. **`birthday` reads back as a full timestamp** anchored near local noon
(`"1815-12-10T11:59:00Z"` from a date-only write), so only the date part becomes
`Anniversary.date` — that field is JSContact date text, which the Google and
CardDAV adapters fill with `YYYY-MM-DD`. **`categories` maps to neutral
keywords** in *both* directions; `contact_write` already emitted it, so the read
mapping is what keeps the round-trip lossless for a field Graph advertises as
supported.

Graph contact writes deliberately advertise `WriteGuard::Absent`: the current
contact update contract documents no enforceable per-object conditional guard.
Personal-contact create/patch/delete are supported and the engine refetches the
canonical contact after a successful outbox write. Organization contacts and
directory users remain read-only. Photos are fetched only on demand.
Birthday and homepage are retained on reads but are not writable capabilities:
Graph exposes one scalar for each while the neutral model permits multiple
anniversaries and links, so choosing one would silently lose intent.
`businessHomePage` is the **only** web address a Graph `contact` carries — the
resource has no personal-homepage counterpart, so there is nothing to pair it
with. (A second mapping here once read `personalNotes`, the notes field, and
republished any note beginning with `http` as a URL resource.)

The capture helper defaults include delegated `Contacts.ReadWrite`,
`OrgContact.Read.All`, `User.ReadBasic.All`, and `ProfilePhoto.Read.All`.
Directory permissions can require administrator consent and must never become a
prerequisite for personal contact sync.
