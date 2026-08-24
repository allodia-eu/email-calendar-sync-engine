# `engine-api` — the host facade

`engine-api` is the stable, host-facing entry point to the engine (`north-star.md`:
*"Host-facing APIs live behind `engine-api`."*). It is the **one composition
point**: instead of wiring `engine-store`, `engine-sync`, the providers, a search
layer, and a clock together, every host — mobile (UniFFI), desktop/daemon (the C
ABI), the CLI, and server adapters — drives the engine through this crate.

This doc is authoritative for the facade's shape and the order its slices land.
Read it before touching `engine-api` or adding a binding/reference-host seam.

## What it is

- An [`Engine`] owns **one durable [`SqliteStore`]** driven by a host wall clock
  ([`SystemClock`]), and exposes high-level operations over it.
- Hosts call `Engine::open` / `open_in_memory` — or `open_with` / `open_in_memory_with`
  with `OpenOptions { fts_tokenizer: FtsTokenizer::Trigram }` when the database must be
  born with the CJK-friendly trigram FTS tokenizer — then `sync_mail` / `sync_calendar`; build a mailbox
  list with `mail_window` (the projected rows a list renders, across any set of accounts
  in one ordered answer), complete its conversations with `mail_on_threads` and resolve
  a named message with `mail_by_keys`; read `mailboxes` / `messages` /
  `calendars` / `events` and `search_mail` / `search_calendar` (which now also
  matches fetched **body** text); open a message with `message_body` (fetch-on-demand;
  caches the raw bytes on disk and the extracted text in SQLite, so reopen is a fast
  SQLite read and the body becomes searchable), plan a bulk body-warming pass with
  `mail_missing_body` (the newest synced messages whose body text is not yet
  cached — a host feeds each through `message_body` to make its window readable
  offline), resolve inline CID resources with
  `message_inline_parts`, list ordinary downloadable attachments with
  `message_attachments`, fetch a selected attachment with `message_attachment`,
  recognize a meeting invitation with `message_scheduling` (the inbound iMIP read —
  cache-first on the same raw source, so it costs no extra fetch; it *reports* what
  arrived and deliberately makes no decision, because whether to offer an RSVP is a
  product rule over the `METHOD` plus an `ATTENDEE` matching one of the account's own
  addresses — see `calendar-semantics.md`); and
  write with `submit_mail` (send) / `edit_mail` (mark-read/flag, move, delete) /
  `create_calendar_event` / `patch_calendar_event` / `delete_calendar_event`
  (+ `put_calendar_document`, the iMIP RSVP escape hatch) / `pending_op_state`.
  Contact hosts use `sync_address_books`, source-bound `sync_contact_cards`, or
  combined `sync_contacts`; browse via generation-bound `people_page` and
  `person`; write one explicit destination with `create_contact` /
  `patch_contact` / `delete_contact`; fetch authenticated media with
  `contact_photo`; and compose recipients with `recipient_suggestions` plus the
  history-forget/clear methods. Unsupported destination fields are rejected
  before enqueue and successful writes refetch the canonical card.
  `contact_destinations(adapters)` enumerates the explicitly **writable** books
  and takes no account: each adapter is already bound to one, so an account
  parameter could only re-state what the caller chose when it assembled the
  list. A read-only source belongs in an address-book listing, never in a "save
  to…" picker. Group membership is a `PeopleQuery` filter, and `contact_photo`
  is fingerprint-cache-first. A JMAP adapter must be rebound to each discovered
  opaque address-book id with `with_contact_address_book` before it is supplied
  as a host-facing destination — unbound, it offers no destination at all.
  `Person::display_name` is an `Option`: a person with no name and no address
  has none, and the host — not the engine — chooses what to call them.
- **A calendar grid reads `occurrences_in`, not `events`.** `events` returns the
  projected envelope — a recurring series is one object, at its series start — so a host
  that lays *that* out shows a weekly meeting in exactly one week. `occurrences_in(account,
  window)` returns the materialized instances overlapping a half-open UTC window, each
  pointing back at its master for the title/participants. Pair it with `to_local` /
  `day_bounds_utc` (the only UTC→local direction the engine offers, so a host never
  bundles a second tzdb) to build the window and place a row in a day column.
- **Widen the horizon with `expand_horizon`; a re-sync will not.** Sync expands only what
  its delta *changed*, so reading a window no sync ever materialized returns empty —
  permanently, no matter how often the host re-syncs. `expand_horizon` re-derives the
  stored events over a new window with no network, and is also the path for a display-zone
  or tzdata change. Both it and `sync_calendar` report the events they could **not**
  expand (`unexpandable`): those materialize zero occurrences and so render nowhere, and
  the host is expected to surface that rather than lose them silently.
- **A provider key resolves to one *message*, not to one row.** The store's key is
  `(scope, key)`, and two of an account's mail scopes can hold the same key: a Microsoft
  Graph move keeps the message's immutable id and Graph mail sync is per folder, so between
  the destination folder's delta and the source folder's, both scopes hold it. `mail_by_keys`
  therefore returns **two rows** in that window — a host reading rows sees the store's truth —
  while `messages` / `messages_by_keys` return **one** `Message`, composed from the row with
  the later `last_modified`. The payload is the message's immutable half and is the same bytes
  in either scope, so only the row's filing differs, and the later modification time is the
  move. Do not "simplify" `compose` back into a `HashMap` collect: that resolves the duplicate
  by whichever scope the read visited last, which is a coin flip between the old folder and the
  new one (`tests/sync/folder_scopes.rs` asserts both sort orders for exactly this reason).
  The read
  surface enumerates the account's scopes and filters by `SyncScope::object_kind`, so
  the facade never hard-codes which scopes a provider uses. The return values (e.g.
  `MailSyncReport`, `Vec<Message>`, `Vec<Event>`, `SearchResults`, `SubmitOutcome`) are
  the host's feedback.
- Providers are **host-constructed**, not owned by `Engine`: the host builds each
  provider — passing one shared `engine_tls::TlsClientConfig` for the account
  (`tls.md`) — and hands it to `sync_*`. Exposing the `TlsPolicy` over the bindings
  is a later slice.

## What it is not

- It is **not** a second home for domain logic. Normalization, projection,
  recurrence expansion, the store contract, and sync orchestration stay in their
  crates; `engine-api` only composes them.
- It is **not** provider-aware. It never switches on protocol or names a concrete
  provider — see the provider-agnostic invariant below.

## Key decisions

- **Concrete store, not `dyn Store`.** SQLite is the engine's first store, and the
  search and other conveniences live on `SqliteStore` (inherent methods), not on
  the `engine_store::Store` trait. The facade therefore holds a concrete
  `SqliteStore<SystemClock>`. Other stores are host adapters; if a second store
  ever ships, that is the point to introduce a store-selection seam, not before.
- **Store-creation options are fresh-only: `open_with` / `open_in_memory_with`.**
  `OpenOptions { fts_tokenizer }` selects the FTS5 tokenizer both FTS tables are
  created with (`search.md`); the default (`porter unicode61`) is what `open` /
  `open_in_memory` keep using. The options shape **a database the call itself
  creates** — an existing one carries the tokenizer recorded in
  `meta.fts_tokenizer` at its own creation, and an open that requests a different
  tokenizer fails with `ApiError::Store` (a `StoreError::Backend` naming both
  values and the recreate-and-re-sync recovery). There is no re-tokenization in
  place anywhere in the engine, so a host switching tokenizers recreates the
  database and lets sync re-derive its contents. Passing options is therefore a
  first-launch decision, not a setting.
- **The wall clock lives here.** `engine-store` ships only `ManualClock` for
  deterministic tests and never reads wall-clock time itself; the engine's time
  source stays one injected seam. `engine-api` supplies the real one
  (`SystemClock`, built from `time::OffsetDateTime::now_utc()`, whole-second
  resolution — enough for lease liveness; it is a wall clock, so cross-step
  ordering rests on the TTL + `StaleLease` reclaim, not on the clock). It is
  crate-internal (`pub(crate)`) for now — nothing public accepts a clock — and
  becomes public when a clock-injection constructor lands (see deferred seams
  below). Keep new real-world I/O seams (clock, later: network policy, blob roots)
  on this side of the boundary.
- **Generic over `Provider`.** `sync_*` take `&impl Provider`, so the facade is
  provider-agnostic and a host passes a `provider-jmap` / `provider-imap` /
  `provider-caldav` adapter. (The `engine-sync` free functions are generic over
  `P: Provider`. A host that picks a concrete adapter at runtime can hold a
  `Box<dyn Provider>` and still call them: `engine-provider` provides a blanket
  `impl<P: Provider + ?Sized> Provider for Box<P>` that delegates every method to the
  box's contents — kept there, not special-cased in `engine-api`.)
- **Host-config is hardcoded in this slice, by design (deferred seams).** An
  `Engine` stamps a fixed `WorkerId` (`"engine-api"`), uses a fixed `LEASE_TTL`
  (5 min — a generous safety bound, not a deadline; the sync loop re-claims and
  recomputes on `StaleLease`), and constructs its own `SystemClock`. The durable
  docs describe all three as host-controlled seams — host-assigned worker identity,
  a *"TTL (host-tunable via the injected clock)"* (`store-and-sync.md`), and an
  *"injectable clock/time source"* (`north-star.md`) — and the engine layers below
  honor them; the **facade just does not expose them yet**. Host-supplied worker id
  (for multi-device lease attribution), host-tunable TTL, and clock injection (for
  deterministic facade tests) are deferred to a later slice; threading them through
  `open()`/`sync_*` then is an additive change. Until then, fencing tokens (not the
  worker id) still serialize writers correctly.
- **Concurrent same-scope syncs resolve to `Busy`, not corruption.** `Engine` is
  `Send + Sync`; share one as `Arc<Engine>`. Two syncs of *different* scopes run in
  parallel, but two of the *same* `(account, scope)` cannot both hold its lease: the
  store returns the retryable `ScopeHeld`, the sync loop surfaces it (it recovers
  only `StaleLease`), and the facade maps it to `ApiError::Busy` — a distinct,
  retryable signal separate from `ApiError::Sync`. The facade does **not** itself
  queue or auto-retry; a host serializes per account or retries on `Busy`. If a
  future slice wants transparent serialization, add a per-account async lock in the
  facade — do not widen `run_scope` to swallow `ScopeHeld`.
- **Abrupt process recovery is explicit.** A host that knows prior workers for the
  store are gone after process death can call `Engine::abandon_sync_leases` once at
  startup. It clears held scope leases and bumps their fencing tokens while
  preserving cursors, so a cold backfill resumes from its last committed checkpoint
  immediately instead of waiting for the fixed `LEASE_TTL` or clearing state. This
  is not a normal `Busy` recovery path for live in-process contention.
- **Re-export signature types.** Types that appear in the facade's own signatures
  (`AccountId`, `TimeZoneId`, `Horizon`, the sync reports, `Provider`, and the
  streaming vocabulary — `StreamTuning`, `SyncObserver`, `SyncCommit`, `IgnoreCommits`,
  `AccountProgress`, `ProgressSnapshot`, `SyncScope`, `SyncWindow`, `CalendarDate`,
  and the store-creation options `OpenOptions` / `FtsTokenizer` that
  `open_with` / `open_in_memory_with` take) are
  re-exported so a host depends on `engine-api` alone. The concrete provider still
  comes from the adapter crate.
- **Display-side timezone resolution.** `resolve_instant` / `resolve_instant_in` /
  `is_supported_zone` (with `ExpandError`) are re-exported from `engine-recurrence`
  so a host can resolve a stored event's start to its absolute UTC instant for
  local-zone display (`resolve_instant`), get a total-order sort key for a
  mixed-kind agenda in a chosen display zone (`resolve_instant_in`), and validate a
  picked/device zone before adopting it (`is_supported_zone`) — without depending on
  `engine-recurrence` or bundling tzdata itself (`calendar-semantics.md`).

## Slice plan

Step 6 lands in small, tested slices. Order and status:

1. **Lifecycle + provider-driven sync — _done_.** `open`/`open_in_memory`
   (+ the creation-options variants `open_with`/`open_in_memory_with`, whose
   fresh-only `OpenOptions` semantics are a key decision above),
   `sync_mail`, `sync_calendar`, `SystemClock`, and `ApiError`.
2. **Per-account search — _done_.** `StoreRead::account_scopes(account)` enumerates
   an account's scopes (a `SELECT … WHERE account = ?` over `sync_scope`, each JSON
   `scope_key` decoded back to a `SyncScope`; contract-tested in `engine-store`, so
   both the in-memory store and `store-sqlite` satisfy it). `Engine::search_mail` /
   `search_calendar` parse the DSL, filter the account's scopes to the queried
   domain via `SyncScope::search_domain` (so the facade never hard-codes a
   provider's scopes nor branches on protocol), and run them through the store's
   executor — returning `SearchResults` with coverage. A malformed query string is
   `ApiError::Query`.
3. **Writes / outbox — _done_.** `Engine::submit_mail` drives `engine-sync`'s outbox
   `submit_mail` (durable op → claim → provider send → record), returning a
   `SubmitOutcome` (sent key, `Message-ID`, op id); a failed send is recorded
   `Failed` / `NeedsConfirmation` *before* surfacing as `ApiError::Sync`, so the
   outbox never blind-retries. `Engine::pending_op_state` exposes
   `StoreRead::pending_op_state` for polling an op's lifecycle (e.g. confirming an
   ambiguous send). `Engine::edit_mail` rides the same outbox for mail mutations —
   it takes a caller-minted idempotency key and a `MailEdit` (mark-read/flag, move,
   or permanent delete) and returns a `MailEditOutcome` (resolved key + op id); a
   failure (e.g. a stale-target `Conflict`) is recorded `Failed` before surfacing as
   `ApiError::Sync`. `Engine::create_calendar_event` / `patch_calendar_event` /
   `delete_calendar_event` ride the same outbox for calendar mutations — a caller-minted
   idempotency key plus an `EventDraft` (the event you want), or the event **as you read
   it** plus a `PatchTarget` + `EventPatch` (what changed, and on which occurrence), or an
   `EventDeletion` — returning a `CalendarWrite` / `CalendarDelete`. These carry **intent**: the host never assembles
   iCalendar, mints an href, or touches an `ETag`, and the same call drives CalDAV and JMAP
   (`providers.md`). The write types are re-exported from `engine-api`.
   - **Read `Capabilities::calendar_write_guard()` before writing.** `WriteGuard::Enforced`
     (CalDAV) means a stale edit is refused — a `412` surfaces as a `Conflict`, to be
     recovered by re-syncing and re-applying, never a blind retry. `WriteGuard::Absent`
     (JMAP) means the transport **cannot** refuse one: a stale edit silently wins, so a
     successful write does not imply no concurrent edit was lost, and a host that cares must
     detect it itself (`jmap.md`).
   - **Read `Capabilities::calendar_scheduling()` before offering an RSVP, and
     `scheduling_submission()` before composing an iMIP message** (issue #105). The first
     says whether the *server* delivers the iTIP the answer implies — discovered on CalDAV,
     constant elsewhere; the second says whether this transport can send one itself.
     Together they answer "can this account answer an invitation at all?", which no single
     flag does: on a plain CalDAV calendar, `rsvp_calendar_event` stores the right
     `PARTSTAT` and the organizer learns nothing. A host that reads neither ships the exact
     silent success the RSVP verb was designed to prevent.
   - **`put_calendar_document` can create, not only replace.** `EventWrite::creating(…)`
     asks the server to store the document **only if nothing is there** (`WritePrecondition::IfAbsent`),
     so putting an invitation that arrived as mail onto the calendar is a guarded create: a
     resource that appeared in the meantime is a `Conflict`, never a silent overwrite. This
     is the path for an inbound invitation specifically because
     `create_calendar_event`/`EventDraft` carries neither organizer nor attendees and would
     store a plain appointment with nothing to answer on.
   - **A calendar write reconciles the store before it returns** (issue #65). A write's
     response is a *receipt*, not a document (a CalDAV `PUT` answers with an `ETag` and no
     body; a JMAP `/set` with an id and no object), so the driver alone would leave the row
     holding the **pre-write** projection, `raw_ical` and revision. Each facade write
     therefore runs `engine_sync::reconcile_calendar_events` — an **event-scope delta**, one
     round trip, the same primitive a sync reads through — the moment the write lands. The
     store then holds what the **server** holds, a delete is tombstoned locally, and an edit
     that moved the event moves its occurrence rows. That is what makes "edit, re-read, edit
     again" work: the second edit's guard is the revision the *server* reported, not the
     superseded one it wrote over. Proven live against Stalwart (CalDAV + JMAP) and SabreDAV.
     - **A write is never told what the UI is showing.** The reconcile re-expands over the
       window the *store* holds (`ExpansionWindow`), so the write methods take no `horizon`
       or `host_zone`, and a write can neither widen nor narrow what the host has expanded.
       `Engine::expand_horizon` owns the window; see `store-and-sync.md`.
     - **Never store our own bytes instead.** The reconcile must re-read from the server:
       Stalwart *reserializes* what it stores, so an optimistic local copy would put a
       `RawIcal` in the store the server does not have — and would **mask a server that
       silently dropped a property** (`caldav.md`). Body and revision also cannot move
       independently: a row claiming a revision whose bytes it does not hold lets a host
       patch a stale body under a valid guard and silently revert its own edit.
     - **A write that did not reconcile is still a write.** The reconcile is a *local* step
       after a write the server already accepted, so it can never fail the write: it is
       reported as `Reconciled::{Applied, Busy, Failed}` on the outcome, never as an error.
       `Busy` means a concurrent sync holds the event scope. Recover by re-reading
       (`Engine::reconcile_calendar_events`, also the batch path for a host driving the
       low-level `engine_sync` drivers itself) — **never** by re-issuing the write.
4. **Mail sync — _one entrypoint, and the engine owns the fan-out._**
   `Engine::sync_mail(providers, account, tuning, observer)` takes the account's mail
   providers — one per folder where the protocol binds a connection to a mailbox (IMAP),
   a single element where one serves the account (JMAP, Gmail, Graph) — and runs the
   whole pass: the folder-list container once, the account-level store steps once, then
   the folders **concurrently, bounded, Inbox first**.

   Each folder commits **chunk by chunk** under its own lease, reporting a `SyncCommit
   { scope, fetched, total, upserted, removed }` after each committed chunk — so a UI
   shows recent mail before the sync finishes **and** splices its list from the exact
   rows without re-querying. An additive pass checkpoints the cursor per chunk, so a
   mid-stream crash resumes where it stopped; a reconcile re-snapshot holds the cursor
   until its tombstoning final chunk (`store-and-sync.md`). `StreamTuning` sets the
   per-sync depth `window` and decouples the fetch batch (round trips) from the chunk
   size (commit granularity).

   The observer also receives the pass's **lifecycle** — `account_sync_started(folders)`
   once the denominator is known, `folder_sync_finished` per folder, and
   `account_sync_finished` — which is what a host renders "syncing, 5 of 10" from. All
   three default to nothing, so an observer that only splices rows implements
   `committed` alone. A closure is a `SyncObserver` via the blanket impl, and
   `IgnoreCommits` is the no-op sink.

   Each `FolderSync` also carries a `SyncTiming` — how long that folder spent **fetching**
   (the network), **deriving** (projecting rows) and **storing** (the apply). They do not
   sum to its `elapsed`; the remainder is the scope lease and the bookkeeping between
   chunks. Reported rather than logged, for the reason in `AGENTS.md`: a log line is the
   host's product surface, a duration is a fact, and this is the same seam
   `ConnectObserver` uses.

   **It returns a `MailSyncReport`, not a `Result`**, because a partial failure is the
   ordinary case: `account_steps` (a **store** fault, never a network one), `mailboxes`,
   and one `FolderSync` per folder with its own result and elapsed time. Collapsing
   those into one error loses what a caller needs to tell an outage from an expired
   sign-in from a scope another pass is holding — `SyncError::is_busy` names the last.

   **Why one and not four.** There used to be a whole-account convenience, a streaming
   variant, and the two halves a host drove itself; the shipping client used only the two
   halves. So anything account-level had to be written into functions that never call
   each other, and work put in the convenience ran in the tests and nowhere else — which
   is exactly how the thread-index repair came to be unreachable in the product while
   every test passed.

5. **Targeted refresh — `Engine::refresh_folders(providers, account, tuning, observer)`.**
   Syncs exactly the folders given and **discovers nothing**: no folder-list sync, no
   repair, no recipient backfill, no coverage record, and `mailboxes` in the report is
   `None`. For a caller that already knows which folder changed — an `IDLE` push, a
   webhook, a folder the user just opened.

   It is a **different operation, not a second way to run a pass**, and that distinction
   is what keeps it from re-creating the problem above: its contract is that it does no
   account-level work, so anything that must happen once per account goes in `sync_mail`
   and only there, beside the repair and the recipient steps.

   It exists because discovery is most of what a targeted refresh would otherwise pay.
   Measured on a steady-state single-folder pass against a live server, the folder list
   was **57%** of the work with `LIST-STATUS` and **86%** without — and in round trips,
   which is what a remote server charges for, a server that cannot answer `LIST-STATUS`
   is asked for a `STATUS` **per folder**: one extra trip becomes fourteen on a
   thirteen-folder account, on the path whose job is making new mail appear at once.
5. **Bindings.** `bindings-uniffi` (Kotlin/Swift) and `bindings-ffi-c` (C ABI)
   over `engine-api`. These need `unsafe`/codegen, so they override the workspace
   `unsafe_code = "forbid"` lint locally (isolated + documented, per `AGENTS.md`),
   and they pick concrete provider/clock types — `engine-api` stays idiomatic Rust.

When a slice migrates the CLI onto the facade, reconcile `engine-cli`'s docs (its
lib already anticipates *"When `engine-api` lands, the CLI will consume that stable
facade"*).

## Invariants for the next agent

- **Keep it provider-agnostic.** No protocol branching, no naming a concrete
  provider crate in a dependency or signature. New provider behavior belongs in a
  provider crate behind the `Provider` trait.
- **Keep it a thin composition.** If a method grows real logic, that logic
  probably belongs in `engine-sync`/`engine-search`/`engine-core` with a test
  there; the facade just calls it.
- **Errors wrap, never restring.** `ApiError::Store`/`Sync` carry the underlying
  engine error unchanged so its `source()` chain (provider failure class, store
  backend detail) stays inspectable. The one deliberate exception is `ScopeHeld`,
  which `map_sync_error` classifies as `ApiError::Busy` (a retryable race, not a
  failure) — classification, not restringing. Add similar classifications there if
  another error class deserves a distinct host signal.
- **The clock is a wall clock, not monotonic.** `now()` is whole-second and can
  step backward (NTP); do not write code or tests that assume monotonic `now()`.
  Lease safety across a step rests on the TTL + `StaleLease` reclaim in the sync
  loop, not on the clock.

## Verification

The crate's deterministic tests cover it without the Stalwart harness: an
end-to-end `tests/sync.rs` opens an `Engine` and syncs mail+calendar through a
**cursor-aware** fake `Provider` (snapshot first, delta after), the same way a host
would. From the returned reports it asserts: a first snapshot upserts; a resync
after reopening a file-backed store is an *empty delta* (proving the cursor — and
data — persisted, since a lost store would re-snapshot and upsert); a delta that
drops a key tombstones it; a provider failure surfaces as `ApiError::Sync` and a
bad path as `ApiError::Store`; and two concurrent syncs of one scope resolve to
`ApiError::Busy` (a `tokio::sync::oneshot` gate holds one sync's lease while the
other races, deterministically — no timing). The same file's search tests then
exercise per-account search over the synced data: a DSL query finds the matching
mail/event with complete coverage, a malformed query is `ApiError::Query`, and an
unsynced account returns an empty answer. A `SubmittingProvider` then exercises the
outbox facade: a successful `submit_mail` commits the op `Succeeded` (read back via
`pending_op_state`), a failed send surfaces as `ApiError::Sync`, and an unknown op id
reads back `None`. A `sync_mail` with a closure observer then asserts
one `SyncCommit` lands with `fetched == total == 2`. Run the standard gate (`AGENTS.md`):
`cargo +nightly fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D
warnings`, `cargo test --workspace --all-features`, `cargo doc`. `engine-api`'s own
lines run ≈94% under the offline metric (no live provider needed); the uncovered
remainder is the file-backed `Engine::open_with` shim — the store constructor it
forwards to, including the tokenizer-mismatch refusal, is covered in
`store-sqlite` — plus streaming stretches in `engine/sync.rs` that predate the
open-options work.

The fake `Provider` and object builders in `tests/sync.rs` are a third copy of a
pattern `engine-sync` and `engine-provider` also hand-roll as crate-private test
code. Promoting one shared fake + builders behind a `test-support` feature/module
(so the `Provider` trait has a single fake to update) is a worthwhile follow-up,
deferred here to avoid refactoring three crates' tests in this slice.

[`Engine`]: ../../crates/engine-api/src/engine.rs
[`SystemClock`]: ../../crates/engine-api/src/clock.rs
[`SqliteStore`]: ../../crates/store-sqlite/src/lib.rs
