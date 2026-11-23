# Windows and widening

Status: design note, not implemented. Direction approved by the owner: **windows are neutral and
widen without a re-read.** Changes to public types and to `Provider` named here still need the
owner's OK before they land (`AGENTS.md`, "Breaking changes").

This note fixes three things: a neutral vocabulary for "how far back", so no host does month
arithmetic of its own; a **widening** operation that fetches only what a wider window newly
admits, keeps the cursor, and resumes after a crash; and **narrowing** as its symmetric operation,
so neither direction needs a cursor cleared.

It builds on what exists: `SyncWindow` and `MailboxWindows` (`engine-core/src/sync/window.rs`),
the depth recorded with each email cursor (`engine-sync/src/window_cursor.rs`), the delta filter
(`MailboxWindows::admits`, moving into `EmailChunk::into_update` in PR #374), the local prune, and
on the calendar side `Horizon`, the stored expansion and fetch windows, and `reach_calendar`
(`store-and-sync.md`, "The fetch window is the store's too").

## 1. Hosts

Three kinds of host use the engine. The design must serve the first two and cost the third nothing.

| | Sync host (an app syncing into the engine's store via `engine-api`) | Transfer host (a migration tool driving `Provider` directly into its own ledger) | Direct host (paged queries, no store) |
|---|---|---|---|
| Mail window | Per account, in months: a default (3 mobile, 6 desktop), steps 3, 6, 9, 12, 24, all. Some mailboxes held further back for a while (Sent, for a writing-style read). | Usually the whole history. Sometimes an admin window ("last 2 years"). A trial window ("last 3 months") that later widens. | None. |
| Calendar window | A rolling horizon (−120/+400 days today), widened in 91-day steps toward pages the user visits. | Every event with an occurrence on or after a floor, the whole series included. | None. |
| Widening today | Clears every mail cursor and re-syncs the account: every folder re-snapshots its whole new window. Calendars: `reach_calendar` reopens over the union, then `expand_horizon`. | Reopens the source cursor over the union as a fresh snapshot; the ledger keeps it from writing twice. | n/a |
| Must hold | Old mail appears without re-reading recent mail; the list stays usable during the pass; a killed app resumes. | **No duplicate writes on the target**, no item missed between trial and full window, resumable after a lost worker, no spurious "removed" from a partial enumeration. | Nothing new required; no signature it uses changes. |
| Narrowing | Prunes locally, then clears cursors and re-syncs (on a depth reduction, and on releasing older Sent mail). | Rare; the ledger keeps what it wrote. | n/a |
| Does its own date arithmetic | Yes: whole calendar months back with the day clamped to the target month (Mar 31 − 1 month = Feb 28). | Yes, the same rule, written again. | No. |

What both hosts lack is the same thing: a way to say "I held `[A, ∞)`, now give me `[B, A)`" and
keep the cursor they have.

## 2. Survey

Every adapter with a mail or calendar surface, read from the code on `main` (468c841). "Measured"
means a live test in this repository pins it; "documented" means the provider's own docs say so and
nothing here has re-measured it.

### Mail

| Adapter | How the window is applied | Does the cursor encode the window? | What widening costs today | A band `[new, old)` would take |
|---|---|---|---|---|
| IMAP (`provider-imap`) | Snapshot or backfill: `UID SEARCH SINCE <floor>`, then `UID FETCH` of those UIDs newest first. Delta: no date filter (new UIDs, `CHANGEDSINCE`/`VANISHED` with QRESYNC; without it, flags of held mail read through the same `SEARCH SINCE`, `resync.rs`). Yahoo's `MESSAGELIMIT` cut falls back to reading `INTERNALDATE` per message (`transport_search.rs`). | No. The cursor is `UIDVALIDITY`, `UIDNEXT`, `HIGHESTMODSEQ`, a reconciled mark; `backfill_low` is a **UID** watermark for a cold backfill still descending, not a date. The orchestrator stamps the floor around it (`window_cursor`). | The earlier floor makes `window_cursor::resume` start the folder over: a fresh backfill re-fetches the metadata of every message in the new window, the old window included, and ends in a reconcile. | `UID SEARCH UID 1:<UIDNEXT−1> SINCE <new> BEFORE <old>`, then the existing grouped fetch. `SINCE` and `BEFORE` are complements on the server's own `INTERNALDATE` clock, so the band meets the held window with no gap and no overlap whatever the server's zone. Keeps `UIDNEXT` and the modseq. Resume: a band watermark like `backfill_low`. The `SINCE … BEFORE …` shape is already sent to Gmail over IMAP (`gmail_read.rs`); not yet to Stalwart or Dovecot. |
| JMAP (`provider-jmap`) | Snapshot: `Email/query` with `{after: <floor>T00:00:00Z}`, `OR`-joined with `{inMailbox, after}` per deepened mailbox (`email_window.rs`), walked by anchor with a closing id listing (`walk.rs`). Delta: `Email/changes`, no filter. | No. The cursor is the `Email` state read with the first page; the orchestrator stamps the floor. | The account's one `Email` scope starts over: the whole new window is walked and fetched again, every mailbox, every message already held. Deepening one mailbox costs the same. | `Email/query` with `{after: <new>, before: <old>}` (RFC 8621 §4.4.1: `after` is "same or after", `before` is strictly before, so the band and the held window are exact complements), plus `{inMailbox, after, before}` per deepened mailbox. The same anchored walk. Keeps the `Email` state, which predates the band, so every change to a band message after the walk is replayed. `before` is a new request shape here. |
| Gmail API (`provider-google`) | Backfill: `messages.list` with `q: after:` (account) and `labelIds` + `q: after: before:` per deepened label (`window_listing.rs`), a page at a time, deltas from the stored historyId between pages. Then a reconcile pass that lists ids only. Delta: `history.list`. | No. The historyId is account-wide and window-free. The cursor records backfill progress: `Backfill { history, label, page }` (`cursor.rs`). | The scope starts over: every message in the new window is fetched again at 20 quota units each (about 270 a minute), so widening 3 → 24 months pays for the 3 months held as well. | Already nearly there: a listing `{label: None, after: <new>, before: <old>}` is the shape a deepened label uses today, and the `Backfill` cursor already resumes a listing by page token while keeping the historyId. `after:` and `before:` are both inclusive of the instant they name (measured), so the boundary instant is listed twice; the store writes it once. |
| Gmail over IMAP (`provider-google` + `provider-imap::GmailMail`) | All Mail, Spam, Trash searched with `SINCE` (and `BEFORE` for a deepened label, by `X-GM-LABELS`); delta by `UIDNEXT` and `CHANGEDSINCE`; reconcile when counts drop. | No (per-folder `UIDNEXT`/modseq). | Snapshot of the whole new window, as one reconcile. | `SINCE <new> BEFORE <old>` in each of the three folders; the shape exists for deepened labels. |
| Graph mail (`provider-graph`) | Snapshot: `messages/delta?$filter=receivedDateTime ge <floor>`, per folder. Delta: the `deltaLink`, which carries the filter and takes no other. | **Yes**: the `deltaLink` embeds the filter. Whether it reports changes to messages older than its floor is **not measured**, and is not settled by any document this note relied on. | The folder starts over: a fresh `messages/delta` over the new window re-lists every message's metadata. | If the `deltaLink` is window-bound, a band list (`messages?$filter=receivedDateTime ge <new> and receivedDateTime lt <old>`) would never be kept current, and message delta documents only `ge`/`gt` on `receivedDateTime`, so no second delta can be opened over a closed band. Widening is then a **reopen** over the new floor, as today. Measure first (§9). |
| CalDAV, CardDAV (`provider-caldav`) | No mail. | — | — | — |

### Calendar

| Adapter | How the window is applied | Does the cursor encode the window? | What widening costs today | A band would take |
|---|---|---|---|---|
| CalDAV | Not at all: `sync-collection` reports a calendar whole; `sync_events` ignores the window. | No. | Nothing to fetch: `reach_calendar` answers `Covered`; `expand_horizon` materializes the new days. | Nothing. |
| JMAP | Not at all: `CalendarEvent/changes` reports the account's events whole. | No. | As CalDAV. | Nothing. |
| Graph | `calendarView/delta?startDateTime=…&endDateTime=…`, a mandatory range. | **Yes**, and measured (`provider-graph/tests/live_calendar_window.rs`): the `deltaLink` reports changes only inside its range. The store records the range with the cursor (`fetch_window`). | `reach_calendar` reopens over the union of the recorded window and the range: the whole union is listed again. | A second `calendarView/delta` over the band would be kept current, but a range-bound feed reports an event that moves out of its band as removed while it may now sit in the other band (expected, not measured; §6.2). |
| Google Calendar | `events.list` with `timeMin`/`timeMax` on the snapshot; `syncToken` after. | The token continues a bounded snapshot; the store records the range. Measured: the token **does** report changes past its bounds, and never lists an unchanged event that was there before (`provider-google/tests/live_calendar_window.rs`). | Reopen over the union, as Graph. | `events.list` over the band with no token, keeping the token: correct only because of the measured, undocumented behaviour above. |

What the survey says:

- **No mail delta can carry a date filter, on any protocol.** So the delta keeps its cursor and the
  engine judges arrivals (`admits`); this is already neutral and stays.
- **The cost of widening today is the same on every mail adapter: the whole new window is read
  again.** The cause is neutral, not per adapter: `window_cursor` has one answer to an earlier
  floor, which is to start over.
- **Four mail paths out of five can fetch a band and keep their cursor** (IMAP, JMAP, Gmail API,
  Gmail over IMAP): their cursors do not encode the window, and their protocols have a
  complementary pair of date predicates. Graph mail probably cannot, for the same reason Graph
  calendars cannot: a window-bound token.
- **Calendars split by transport, not by host:** whole-calendar transports never need a band;
  window-bound ones need a reopen (Graph) or may use a band on measured behaviour (Google).

So the fix lands in the neutral layer (the vocabulary, the band computation, the cursor record,
the orchestration), with one adapter method each transport answers in its own way and a default
that reproduces today's behaviour exactly.

## 3. Vocabulary

### 3.1 Month arithmetic moves into `engine-core`

```rust
impl CalendarDate {
    /// `months` whole calendar months before `self`, the day clamped to the target month's
    /// length (Mar 31 − 1 = Feb 28, or Feb 29 in a leap year). `None` below the representable range.
    pub fn checked_months_before(self, months: u32) -> Option<CalendarDate>;
    /// `days` days later (negative: earlier). `None` outside the representable range.
    pub fn checked_add_days(self, days: i64) -> Option<CalendarDate>;
}

impl SyncWindow {
    /// Mail on or after the day `months` calendar months before `today`. A floor below the
    /// representable range is the full history. `months == 0` is a floor of `today`.
    pub fn months_back(months: u32, today: CalendarDate) -> SyncWindow;
    /// Whether everything `other` admits, `self` admits too (the rule `window_cursor` uses now).
    pub fn covers(self, other: SyncWindow) -> bool;
}
```

The clamping rule is the one the sync host ships today, moved, not changed. "All mail" stays
`SyncWindow::full()`; the engine takes no sentinel month count. `today` is an argument: the engine
reads no clock here, and a test names its day.

A floor computed from today moves forward every day. That must never look like a change of depth,
and it does not: a later floor resumes (`window_cursor`), and §5.3 keeps it that way.

### 3.2 Mail: `Widening` and its bands

```rust
/// Going from the windows a store covers to wider ones.
pub struct Widening { from: MailboxWindows, to: MailboxWindows }

impl Widening {
    /// `None` when `to` admits nothing `from` does not (equal, narrower, or the floor moved on).
    pub fn new(from: MailboxWindows, to: MailboxWindows) -> Option<Widening>;
    pub fn from(&self) -> &MailboxWindows;
    pub fn to(&self) -> &MailboxWindows;
    /// What `to` admits and `from` does not, as listings an adapter can send.
    pub fn bands(&self) -> Vec<Band>;
    /// The one band a folder-bound scope fetches, if its folder gains anything.
    pub fn band_for(&self, mailbox: &MailboxId) -> Option<Band>;
}

/// Mail dated in `[since, before)`, in one mailbox or in any.
pub struct Band { pub mailbox: Option<MailboxId>, pub since: Option<CalendarDate>, pub before: CalendarDate }
```

The bands are computed by one rule, so no adapter derives them:

- **Account band:** `[to.account, from.account)`, any mailbox, when the account floor moved back.
- **Mailbox band**, for each mailbox `m` that `to` deepens:
  `[to.window_for(m), min(from.window_for(m), to.account))`, when not empty. The upper end stops
  at the new account floor, because everything above it is in the account band or already held.
- A floor of `None` is the full history; an old floor of `None` means there is nothing to widen.

Gmail's `window_listing::listings` and JMAP's `email_filter` already enumerate a whole window in
this shape (the account from its floor, each deepened mailbox from its floor up to the
account's). The band rule is the same rule with an upper end, so the two adapters' copies and the
band computation should become one implementation in `engine-core`.

Properties, tested as properties (§9): for any windows `from ⊆ to`, mailbox set and date,
`to.admits ∧ ¬from.admits ⇔ some band admits`, and no band admits what `to` does not. The account
band and a mailbox band never overlap in date; two mailbox bands may, for a message filed in both,
which is listed twice and stored once.

### 3.3 Calendar: `Horizon` helpers

```rust
impl Horizon {
    /// The whole UTC days `[from, until)`.
    pub fn days(from: CalendarDate, until: CalendarDate) -> Result<Horizon, TimeError>;
}
```

With `checked_add_days` and `checked_months_before`, both hosts build their horizons without
arithmetic of their own: the sync host's `−120/+400` days around today, the transfer host's
"from X months back to a far end". The 91-day step toward visited pages is the sync host's UI
policy and stays there; `Horizon::union` already exists for the step.

### 3.4 One type or two? Two.

Mail and calendar windows stay distinct types, sharing verbs (`covers`, `union`/`wider`, bands)
and one discipline: **the window a cursor was fetched under is recorded with it, in the same
commit.** The reasons:

- **Different units.** A mail window is a date floor compared on a message's date, inclusive, at
  day granularity, because IMAP `SINCE` and Gmail `after:` are day-granular. A calendar window is
  an instant range that an occurrence *overlaps* (`Horizon::overlaps`).
- **Different sides.** Mail is open-ended upward by nature (new mail is always in); a calendar is
  bounded on both sides, and Graph requires both.
- **Different membership.** A message is in or out by one date; an event is in when any occurrence
  overlaps, and is then held as a whole series.
- **Different consumers and mix-ups.** A calendar already has two windows (fetch and expansion)
  with their own rules. One generic `Window` would let a host hand a mail depth to
  `reach_calendar`; distinct types follow the newtype rule.

## 4. Widening mail

### 4.1 The operation

A widening is one pass per email scope that streams **only** what the bands admit, applies it as
ordinary `PassMode::Additive` chunks, and leaves the scope's delta cursor where it was. It adds a
`Provider` method with a default:

```rust
/// Streams what `widening` newly admits for this scope, keeping `cursor`.
/// The default reopens: a fresh pass over `widening.to()`, as an earlier floor does today.
fn widen_email<'a>(&'a self, account: &'a AccountId, cursor: &'a SyncState,
                   widening: &'a Widening, fetch_batch: usize, chunk_size: usize) -> EmailStream<'a> {
    let _ = cursor;
    self.stream_email(account, None, widening.to().clone(), fetch_batch, chunk_size)
}
```

The default is today's behaviour, so every adapter is correct the day the method lands, and each
one moves to a band when it is proven live. **The chunk's `PassMode` says how to apply it**, so no
caller asks which kind it got: a band adapter yields `Additive` chunks, the default yields the
`Reconcile` chunks of a fresh pass. `EmailChunk::into_update(to_window, &mut present)` applies
both unchanged.

Band chunks are `Additive`, not a new mode, on purpose:

- They carry **no `present` set and tombstone nothing.** A band's enumeration covers the band, not
  the scope, so treating its absence as a removal would delete the whole held window. For the
  transfer host this is the "no spurious removed" requirement: nothing becomes `removed_pending`
  from a band.
- They are **filtered by the new window**, as every additive chunk is. The only message that
  filter drops is one the server dated inside the new floor and the engine dates outside it (IMAP
  `INTERNALDATE` in the server's zone): the local prune would drop it too, and with no `present`
  set there is no key left unstored and untombstoned. The same edge exists today in a fresh IMAP
  backfill's additive groups.
- No new enum variant, so nothing that matches `PassMode` breaks, the direct host included.

### 4.2 Per adapter

- **IMAP.** Check `UIDVALIDITY` against the cursor first; a change means the cursor is void and the
  pass is the existing reset re-snapshot, not a band. Otherwise search the folder's band with
  `UID 1:<cursor UIDNEXT − 1> SINCE <since> BEFORE <before>` (`SINCE` omitted for a band reaching
  the full history). Restricting to UIDs below the cursor keeps the band and the next delta
  disjoint: a message arriving during the pass has a higher UID and comes through the delta,
  judged by the new window. Fetch in the existing descending groups. Yahoo's cut search takes the
  same `INTERNALDATE` fallback, with both bounds.
- **JMAP.** Walk `Email/query` over the bands' `OR` filter with the existing anchored walk and
  closing listing. Keep the stored `Email` state: it predates the walk, so any change made to a
  band message during or after the walk is replayed by the next `Email/changes`. A message moved
  into a band mailbox after the walk keeps its id and arrives by delta.
- **Gmail API.** A `Backfill` whose listings are `bands()` instead of `listings(window)`. The
  existing cursor already holds the historyId and the listing and page it is in. Deltas between
  pages, as now. No reconcile pass at the end: the band has nothing to reconcile.
- **Gmail over IMAP.** The IMAP shape in each of the three folders.
- **Graph.** The default (reopen) until the measurement in §9 says the `deltaLink` reports changes
  to messages older than its floor. If it does, a band list with `ge`/`lt` and the kept
  `deltaLink` becomes possible; if it does not, the reopen is the correct answer and stays.

### 4.3 Where the pass resumes

Two records, each owned by the layer that understands it:

- **The orchestrator's stamp** (`window_cursor`) records the depth the scope *covers*, as now, and,
  while a widening is in progress, the *target* depth beside it. The covered depth becomes the
  target only in the commit that ends the band.
- **The adapter's cursor** records where in the band it is, together with the band it refers to:
  IMAP a band watermark beside `backfill_low` (the lowest band UID committed), Gmail its existing
  `Backfill { label, page }`. A position recorded for another band is ignored. An adapter with no
  stable position (JMAP's walk in the first slice, any reopen) holds the cursor until the band
  ends, and a crash re-runs the band; the upserts are keyed, so that costs time, not correctness.

`window_cursor::resume` stops answering "start over" to an earlier floor and answers one of:

| Stored stamp vs. asked depth | Pass |
|---|---|
| Covered depth covers what is asked (today's resume, the daily floor move included) | Delta. Stamp the asked depth. |
| Asked reaches further back; no widening in progress | Delta, then `widen_email` with `Widening::new(covered, asked)`. Stamp target = asked. |
| Widening in progress toward exactly the asked depth | Delta, then `widen_email` again; the adapter continues from its recorded position. |
| Widening in progress toward another depth | Drop the target. If the covered depth covers what is asked: delta only. Otherwise widen from covered to asked, afresh. |
| No cursor, or one the adapter cannot read (a bare cursor is one fetched over the full history, as now) | Fresh pass, as today. |

The delta runs first so new mail shows before old mail, and so the band starts from a current
cursor. Both run under the scope's one lease, as a cold backfill does today, and a `StaleLease`
or a cursor cleared meanwhile stops the band at its next commit like any pass.

### 4.4 `present`, reconcile and deltas around a band

- **Until a band ends, the delta is asked under the covered depth and judged by the target.** A
  delta that enumerates (IMAP without QRESYNC reads held flags through `SEARCH SINCE`; Gmail over
  IMAP lists the window when counts drop) then lists only what the store is meant to hold, and
  never meets band keys it lacks; an arrival the band admits is still kept, whichever of the two
  brings it first. After the band, both are the new depth, so the next reconcile covers the band.
- Nothing in the store outside the new window is touched by a band or by the deltas around it.
- **Duplicates.** Within one band listing a key is yielded once. Across listings a key can appear
  twice (Gmail's inclusive boundary instant; a message filed in two deepened mailboxes); across a
  band and a delta it can appear twice where the protocol cannot split them (JMAP, Gmail). Every
  such repeat is the same key: the store upserts it, `stream_email`'s `counted` set keeps progress
  honest, and the transfer host's ledger dedupes by key and revision. The engine's contract is
  therefore **at least once per key, never a gap**, and the band is built so that repeats are rare
  rather than the rule.
- **Recipient coverage** and any "held since" record move to the new account floor only when every
  mail scope of the account has finished its band. Until then they keep the old floor.

### 4.5 Deepening is widening

A mailbox held further back is a `Widening` whose only band is that mailbox's. On JMAP and Gmail
this replaces today's account-wide restart with one filtered listing of that mailbox's older mail;
on IMAP it fetches the folder's older mail instead of restarting the folder.

## 5. Narrowing

Narrowing is the symmetric operation: drop locally what the new window no longer admits, and make
the recorded depth say so. It needs no provider and no cursor cleared.

1. `prune_account_mail_outside_window(account, windows)` removes what `windows` does not admit, as
   today.
2. **In the same operation, every mail scope's covered depth is lowered to `windows`** (the
   narrower of the two), and a running pass is fenced as `clear_scope_cursor` fences one, so it
   cannot write a deeper stamp back after the prune.

Step 2 is the missing half today. Without it, a cursor still records the deeper depth after its
mail is pruned, so the next deepening resumes as if it held that mail and fetches nothing. That is
why the sync host clears every cursor and re-syncs after releasing older Sent mail. With step 2,
narrowing is a prune, and widening back later is a band.

The covered depth may claim **less** than the store holds (a floor moving forward daily records a
later floor while the older mail stays until a prune). That is safe: a later widening re-lists a
few held days, keyed. It must never claim **more**, which is the invariant step 2 keeps.

Where step 2 lives is open (§11).

## 6. Widening calendars

### 6.1 What stays

`reach_calendar` and `expand_horizon` are already the right two verbs: one fetches, one
materializes, either order. Whole-calendar transports (CalDAV, JMAP) answer `Covered` without a
request, so widening a calendar there is only an expansion.

### 6.2 Window-bound cursors: reopen over the union, not a second cursor

For Graph the choice is between reopening the scope's cursor over the union (today) and opening a
second `calendarView/delta` over the band, keeping both. Reopen stays, because a second cursor
breaks the meaning of a removal: a range-bound feed must report an event that leaves its range as
removed (expected from the delta's contract, not measured here), so an event rescheduled from one
band into the other is a removal on one feed and an addition on the other, in either order. Two
feeds would need per-feed membership for every event, a composite cursor growing by one entry per
step, and a merge rule. A reopen re-lists the union once per step and needs none of that. A host
paging years back pays the union each step; stepping geometrically rather than by a fixed 91 days
bounds the total to about twice the final union.

For Google a band list with the token kept would be correct on measured behaviour (the token
reports changes past its bounds). The engine chose the uniform reopen so the store's freshness
does not rest on undocumented behaviour; this note keeps that choice and lists the band as an open
question, to be decided by a live test that pins both directions.

The transfer host widens calendars exactly as now: `sync_events(cursor: None, window: union)` on a
windowed transport, nothing on a whole one (`Capabilities::windowed_events`), and the ledger
dedupes.

## 7. Surface

### 7.1 Sync host (`engine-api`)

- **Mail: no new verb.** `sync_mail` with a wider `StreamTuning::within(..)` widens: each scope
  finds its covered depth short of the asked one and runs its band (§4.3). The sync host stops
  calling `clear_mail_cursors` to widen, and stops calling it after a prune to narrow.
  `clear_mail_cursors` stays, for a forced re-snapshot.
- **Progress:** `SyncCommit` gains a flag saying the commit came from a widening, so a host can
  label the bar "older mail"; `total` is then the band's size. `MailSyncReport` says per scope
  whether a band finished, so a host knows when the new depth is complete.
- **Narrowing:** `prune_account_mail_outside_window` lowers the recorded depth (§5). The rest of the
  sync host's narrowing path (sweep, vacuum) is unchanged.
- **Depth:** `SyncWindow::months_back(n, today)` replaces the host's arithmetic.
- **Calendar: no new verb.** `reach_calendar` plus `expand_horizon` with horizons built from
  `Horizon::days`.

A `widen_mail` verb was considered and is not proposed: it would be a second way to say what the
window already says, and a host would have to know which to call.

### 7.2 Transfer host (`engine-provider`)

It drives adapters directly and keeps its own ledger, so it uses the same pieces without the
orchestrator:

1. `Widening::new(covered, wanted)`; persist `(covered, target)` beside the source cursor, as the
   orchestrator's stamp does.
2. Per container: `provider.widen_email(account, &cursor, &widening, batch, chunk)`.
3. Apply each chunk with `chunk.into_update(widening.to(), &mut present)`; persist `advance_to`
   with the work, as for any pass (the adapter's band position rides inside it).
4. When every container's stream has ended cleanly, record `covered = target`.

A band chunk is `Additive`, so the ledger plans its items and marks nothing removed; a reopen
(default, Graph) is `Reconcile` over the union, and the ledger dedupes by key and revision as it
does now. The host never branches on the adapter.

### 7.3 Direct host

Nothing changes for it: no existing signature gains a parameter, `PassMode` gains no variant, the
window still defaults to the full history, and a widening happens only when a caller asks for it.

## 8. Invariants

1. **No gap:** after a widening ends, the store holds every message the new window admits that a
   cold pass under the new window would hold, and keeps it current by delta.
2. **No duplicate object:** a key is stored once; repeats across listings are upserts. On the
   transfer host's target, no item is written twice (ledger, unchanged).
3. **No spurious removal:** a band never tombstones, and nothing outside the new window is touched
   by a widening.
4. **Cursor kept:** a band leaves the delta cursor at a point no later than the start of the band's
   enumeration, so every change to a band message after it is fetched is replayed.
5. **Crash-safe:** a pass killed anywhere in a band resumes with invariants 1-4 intact, from its
   last checkpoint where the adapter has one, from the band's start where it does not.
6. **Honest coverage:** the recorded covered depth never claims more than the store holds (it
   becomes the target only when the band ends; a prune lowers it).
7. **Daily floor moves are free:** a floor that moved forward never triggers a band.

## 9. Tests and how each claim is verified

Offline, in the order written (tests first):

- **`engine-core`:** `checked_months_before` over a table (Mar 31 − 1; Feb 29 in a leap year and
  not; Jan 31 − 1 crossing the year; 24 and 1200 months; underflow); `months_back` at the range
  edge. `Widening::bands` as a property test over random windows, mailbox sets and dates: band
  admission ⇔ `to.admits ∧ ¬from.admits`, and a test that the account band and a mailbox band
  never share a date.
- **`window_cursor`:** each row of the table in §4.3, including a target changed mid-band, a
  narrower depth asked mid-band, and a floor that moved forward (`ManualClock`): no band.
- **`engine-sync`, against the fake adapters:** a widening leaves the delta cursor's provider part
  unchanged; tombstones nothing; reports progress against the band total; a stream dropped after
  *k* chunks, and a `StaleLease` mid-band, both resume to the same final store. **Equivalence
  oracle:** sync a fixture under the narrow window, widen, then compare the store with one synced
  cold under the wide window: same keys, same states.
- **Request shape in the fakes:** the IMAP fake asserts the band's `UID SEARCH` criteria; the JMAP
  fake asserts the band filter; Gmail's asserts the `q`. The fakes answer canned bytes whatever
  they receive (`AGENTS.md`), so these asserts are what make an offline run say anything about the
  request.

Live, against real servers:

- **Stalwart (IMAP and JMAP) and Dovecot (IMAP):** plant mail at chosen dates (Stalwart through
  JMAP `receivedAt`, which becomes the `INTERNALDATE`; Dovecot by `APPEND` with an internal date),
  sync three months, widen to six. Assert that only band messages were fetched (count the fetched
  keys), the equivalence oracle above, and that a flag change and an expunge on a band message
  made after the widening arrive by the next delta (invariant 4). Kill the pass mid-band (the
  `process_kill` tests are the pattern) and resume. Plant messages on the old floor's day at both
  ends of it, and run Dovecot with a non-UTC zone, to pin that the `SINCE`/`BEFORE` complement
  leaves no gap at the boundary. `live_mailbox_window.rs` in both crates is the starting point.
- **Prove each check can fail:** drop `BEFORE` (over-fetch is caught by the fetched count), shift
  the band's upper bound by a day (the boundary message goes missing), advance the cursor past the
  band (the flag change is lost). Each must turn a test red.
- **Gmail API and Graph** are not in the local Docker harnesses. They are verified by the
  env-gated `live_*` tests against the test accounts, with tokens from `tools/google-oauth` and
  `tools/graph-oauth`, which the owner runs; captured responses become scrubbed fixtures. Until
  such a run, the note says these paths are unverified.
- **Graph mail, before any band:** prove whether a message `deltaLink` reports changes to messages
  older than its `receivedDateTime` floor. Change an older message and poll the `deltaLink`;
  change an in-window one as the control; pin both directions. Only a positive result opens the
  band path; a negative one is recorded and Graph keeps the reopen.

## 10. Phasing

1. **First slice: the month helper and IMAP + JMAP mail widening.** `checked_months_before`,
   `checked_add_days`, `SyncWindow::months_back` and `covers`, `Horizon::days`; `Widening` and
   `Band` with the property tests; `Provider::widen_email` with the reopen default; the
   `window_cursor` decision table and the target stamp; IMAP band with its watermark; JMAP band
   (re-run on crash). Live on Stalwart and Dovecot. `email_filter` moves onto `bands()`. The
   guidance docs (`store-and-sync.md`, `imap-smtp.md`, `jmap.md`, `engine-api.md`) change in the
   same pull request.
2. **Gmail**, API and IMAP: listings from `bands()`; the `Backfill` cursor without the closing
   reconcile for a band. Live on the Google test account.
3. **Narrowing:** the restamp and fence of §5. The sync host then drops its clears after a prune.
4. **Graph mail:** the measurement of §9, then the band or a recorded "stays a reopen".
5. **Calendars**, only if a host needs it: Google's band, decided by a live test.

After slice 1 the sync host can drop its widening clear for every account, without branching: an
adapter still on the default reopen behaves as the clear did.

## 11. Open questions

1. **Where does the narrowing restamp live?** The stamp format belongs to `engine-sync`; the prune
   is a store operation that takes no lease. Options: move the stamp codec to `engine-store` and
   restamp inside the prune's transaction, or give `engine-sync` a `narrow_mail` that prunes and
   then compare-and-sets each cursor. The first keeps it atomic; the second keeps the store
   ignorant of cursor formats.
2. **JMAP band resume.** Store the walk's anchor in the cursor so a killed band continues, or keep
   re-running it? A band of years on a large account argues for the anchor.
3. **A target changed mid-band** restarts the band from the covered depth. Worth continuing the
   in-progress band and adding the difference instead? Only if hosts change depth often while a
   band runs.
4. **Graph mail `deltaLink` scope** (§9). If it is not window-bound, Graph mail can use a band; if
   it is, nothing cheaper than the reopen exists on that protocol.
5. **Google calendar band** (§6.2): adopt on the measured token behaviour, pinned live in both
   directions, or keep the uniform reopen?
6. **A capability for planning.** Should `Capabilities` say whether an adapter widens by band or
   reopens, so a transfer host can estimate a widening's cost before starting it? It would be a
   fact for estimates, never something a caller must branch on to stay correct.
7. **Gmail's inclusive boundary.** `before:` with the floor's epoch minus one second would make
   the band and the held window disjoint; worth it, or are keyed repeats enough?
