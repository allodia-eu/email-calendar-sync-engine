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

`provider-graph` implements the `engine_provider::Provider` contract for the
**mail read/sync** spine. Layers:

- **`error`** — `GraphError` (`Status`/`Json`/`Protocol`/`Transport`) → the
  engine-neutral `FailureClass`. Graph error bodies are a documented
  `{ "error": { "code", "message" } }` envelope; the `code` is captured for
  diagnostics, the HTTP status drives classification (`401`→auth, `429`→rate
  limit, `410 Gone`→`NeedsResync` for an expired delta token, `5xx`→retryable).
- **`json`/`normalize`** — pure `serde_json::Value` → `Mailbox`/`Message`,
  unit-tested against captured fixtures.
- **`transport`** — a `GraphTransport` seam over bearer HTTP. `HttpTransport`
  (reqwest + rustls) is production; the seam lets the fetch/provider
  orchestration run offline against fixtures. There is **no session discovery**
  (the v1.0 root is fixed); requests carry `Prefer: IdType="ImmutableId"`.
  `GraphClient::with_base` overrides the API origin (a forward proxy, a regional/
  sovereign endpoint, or the test replay server), **rebasing** the absolute
  `@odata.nextLink`/`deltaLink` URLs Graph returns onto that origin so
  link-following stays on the chosen endpoint.
- **`fetch`** — folder-list resolution and the message snapshot/delta + re-fetch
  paging.
- **`provider`** — `GraphProvider`, bound to one folder for email.

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
  directly when it carries `@odata.etag` (the common case) and **re-fetches** only
  the etag-less partials (the store applies whole objects, not property merges). A
  removal is `{ id, @removed: { reason } }` → an inline tombstone. (JMAP differs
  again: `Foo/changes`→`Foo/get` always yields full objects.) The rest of the flow
  follows the doc verbatim: initial `messages/delta` with `$select`, drain
  `@odata.nextLink` to the terminal `@odata.deltaLink` (the persisted cursor),
  following the returned URLs as-is since the token encodes the `$select`.
- **Keyword/revision mapping.** `isRead`→`$seen`, `isDraft`→`$draft`,
  `flag.flagStatus == "flagged"`→`$flagged`; `internetMessageId` is preserved
  bracket-stripped as a threading hint (never identity); `conversationId`→thread
  provenance; `@odata.etag`→`ETag` and (full `GET` only) `changeKey`→`ChangeKey`
  revision tokens; `bodyPreview`→the snippet.

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
  `sync_email_page` `limit`. A follow-up for responsive streaming. (`$top` does
  *not* paginate consumer delta, which is why the header is the right lever.)
- **National clouds aren't auto-rebased.** `with_base` rebasing rewrites only the
  commercial-cloud origin (`graph.microsoft.com/v1.0`); links a national-cloud
  endpoint (e.g. `graph.microsoft.us`) returns would be followed verbatim — fine
  for the replay server and a same-origin proxy, a gap for true national clouds.
- **Snapshot order is delta-defined**, not newest-first (consumer delta has no
  `$orderby`). A streaming newest-first snapshot via the list endpoint is a
  possible later refinement.
- **Mail only.** Calendar, submission, and writes are later slices; the provider
  advertises `mail` capability only.

## Calendar (deferred — design notes from the official delta doc)

When the calendar slice lands, follow Microsoft's
[delta-query-events](https://learn.microsoft.com/graph/delta-query-events) doc.
The `@odata.nextLink`/`@odata.deltaLink`/`@removed` machinery (and the
full-object + `@odata.etag` re-fetch heuristic) in `fetch` is **directly
reusable**; the two real differences shape the slice:

- **Time-windowed.** Event delta is `GET /me/calendarView/delta?startDateTime=…&
  endDateTime=…`, **not** `/me/events/delta` (the unbounded form is beta-only). The
  date range is **mandatory** in v1.0 and the token encodes it. This fits the
  engine's model — `providers.md` already says calendar sync "may be inherently
  time-windowed … surfaced as scoped, possibly-incomplete coverage" — so the slice
  drives the window off the host's recurrence-expansion horizon and reports
  coverage. A new `GraphCalendarWindow`-style scope is likely needed (the cursor
  is per-(calendar, window)).
- **Recurrences are pre-expanded.** `calendarView` returns "single instances or
  occurrences and exceptions of a recurring series" — i.e. Graph expands the
  series, whereas the engine stores a master + `RRULE` and expands locally
  (`calendar-semantics.md`, `engine-recurrence`). The slice must decide: ingest the
  windowed instances as `event_occurrence` rows directly (bypassing local
  expansion for Graph), or fetch masters via `/me/events` and use `calendarView`
  only as the change signal. This tension is the key calendar design call.

## Testing

- **Offline (always green, no network):** the normalizers and error mapping are
  driven by **scrubbed real Graph responses** captured from a throwaway account
  (`tests/fixtures/`, with a `README.md` recording provenance + the scrub). A
  fixture-routing fake transport (`test_support`) exercises the folder/snapshot/
  delta/re-fetch/tombstone/pagination orchestration; a blocking mock HTTP server
  exercises the real reqwest transport and the status/transport classification.
- **End-to-end replay (deterministic, runs in CI, no token):** a fixture-replay
  HTTP server (`test_support::replay_server`) serves the captured responses over
  real HTTP, and `GraphClient::with_base` points the real client at it. One test
  drives the **whole stack** — reqwest transport + `@odata`-link rebasing + the
  folder/snapshot/delta/re-fetch orchestration — without a token, so CI gets the
  real-HTTP coverage scarce live tokens can't provide. This is the primary
  integration test.
- **Live (gated on `GRAPH_ACCESS_TOKEN`, skips otherwise):**
  `tests/live_provider.rs` checks folder role resolution and the snapshot→delta
  cycle against a *real* account — an occasional drift check against the actual
  API, not the CI gate. There is no CI harness (no live account in CI); the token
  is obtained with `tools/graph-oauth` (a standalone PKCE-loopback login + refresh
  helper, outside the engine workspace). Excluded from the offline coverage metric
  via the `ci.yml` `--ignore-filename-regex`, like the other providers' live tests.
