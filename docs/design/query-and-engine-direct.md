# Design: a sorted, paged mail query, and the store-less facade `engine-direct`

Status: **proposal**, not implemented. Nothing here changes code or the guidance docs yet; the
slice that ships each part updates `search.md`, `search-coverage.md`, `engine-api.md`,
`providers.md` and `jmap.md` in the same change (AGENTS.md, "Documentation Currency").

## The problem

The engine answers one kind of mail question today: *rank the store's matches by relevance and
keep the best `limit`* (`Engine::search_mail`, `store-sqlite::search_ops`). Three host kinds need
more than that, and two of them need it from a place the engine cannot answer from at all.

- A **sync host** (an app that syncs a window into the SQLite store through `engine-api`) wants
  search results newest first, in pages. Today it gets the 500 most *relevant* hits per account and
  re-sorts them by date, so a recent match ranked 501st is dropped while older ones show, and there
  is no "show more" (issue #83). `mail_window` and `mail_in_mailbox_between` page by date but take
  no filter.
- A **direct host** (a store-less server-side webmail) keeps nothing locally. Every list, thread,
  body and attachment is a request to the user's server, with that user's credential, per HTTP
  request. Because `provider-jmap`'s request builder, session and blob code are `pub(crate)`, such a
  host today carries a second, thinner JMAP client of its own: `Email/query` (`inMailbox`,
  `receivedAt` descending, `collapseThreads`, `position`/`limit`, `calculateTotal`) with
  back-referenced `Email/get` and `Thread/get`, `Mailbox/get` with thread counts and rights, a
  thread view, `bodyValues` under `maxBodyValueBytes`, and a streamed `downloadUrl` fetch. That is
  the engine's protocol knowledge written twice.
- A **transfer host** (a migration tool) mostly streams (`stream_email`), but its verify and
  spot-check passes ask counted, filtered questions of the source and target servers.

The principle this note follows: **each protocol is known once.** The engine answers the query
from the store or from the server through one neutral query type and one page type, and the JMAP
primitives a host still needs become public, so a host-side copy can be deleted.

## Requirements by host

| need | sync host | direct host | transfer host |
|---|---|---|---|
| answered from | store (server for what the window lacks) | server only | server only |
| sort | date (received) desc; relevance on request | received desc; others where offered | received or none |
| filter | text + DSL operators, one mailbox or all, several accounts | one mailbox; text search | mailbox; header/date for spot checks |
| paging | "show more" (forward); stable while sync inserts | jump to page *n* (position), open-around-a-message (anchor), forward | forward, whole listing |
| total | exact count of matches | exact where offered, else estimate or none | exact (verify) |
| collapse threads | conversation list | yes | no |
| snippets / highlight | yes (from the store's FTS) | yes (server) | no |
| thread view | store (`mail_on_threads`) | server | no |
| body | cached (`message_body`) | partial: text/HTML values capped in bytes | raw source (exists) |
| blobs | cached | streamed, never whole in memory | streamed raw source |
| mailbox counts | from the store | total/unread, messages and threads | totals per mailbox (verify) |
| push | `Watch` (exists) | per user, many users per process | no |
| credentials | bound at connect (today) | per request (`CredentialSource`) | per run |
| writes | outbox (`engine-api`) | direct, host owns retries | direct (append/import) |
| completeness | must say when the window hid matches | the server's own answer | must say when an answer is capped |

## Provider survey

Every adapter that speaks mail, the same surface each time. "Store" is the SQLite store, which
answers the same query for the sync host. RFC references are to RFC 8620 (JMAP core), RFC 8621
(JMAP Mail), RFC 9051 (IMAP4rev2), RFC 5256 (SORT, THREAD), RFC 5267 (ESORT, CONTEXT), RFC 9394
(IMAP PARTIAL), RFC 8970 (IMAP PREVIEW), RFC 3516 (BINARY). "Verify" marks a claim this note has
not yet seen a real server make; the live plan below sends the request that settles it.

| surface | JMAP | IMAP | Graph | Gmail API | Store |
|---|---|---|---|---|---|
| server sort | `sort` on `Email/query`; `receivedAt` required, the rest listed in `emailQuerySortOptions` (§4.4.2). The harness's captured session lists `receivedAt size from to subject sentAt hasKeyword allInThreadHaveKeyword someInThreadHaveKeyword`. No relevance order. | `SORT` (RFC 5256: `ARRIVAL DATE FROM TO CC SUBJECT SIZE`, `REVERSE`) only where advertised; IMAP4rev2 itself has none. Without it: UID order (≈ arrival, broken by `APPEND` of old mail, as a migration does) or fetch `INTERNALDATE` for every match and sort in the client. | `$orderby` (`receivedDateTime`, `sentDateTime`, `subject`, …); with `$filter` the ordered properties must lead the filter or the server answers `InefficientFilter`. With `$search` results are "sorted by the date and time the message was sent" and `$orderby` is refused (community-reported; verify). | None. `messages.list`/`threads.list` take no order; observed newest first by `internalDate`, undocumented (verify, with and without `q`). | Any indexed column; relevance via FTS5 `bm25()` + RRF. |
| paging model | **positional and anchored**: `position` (negative counts from the end), or `anchor` + `anchorOffset` (§5.5); server may cap `limit` and says so. `anchorNotFound` when the anchor is gone. | **positional** with `CONTEXT=SORT`/`ESORT` (`RETURN (PARTIAL 1:50)`, RFC 5267 §4.4) or RFC 9394 `PARTIAL` on `SEARCH`; otherwise the client holds the UID list from `UID SEARCH RETURN (ALL)` and slices it. | **continuation only**: follow `@odata.nextLink` whole. `$skip` appears in it, but the docs say not to manipulate it: it counts items the server walked, not the position in the result. `$top` 1–1000. `$skip` with `$search` refused (community-reported; verify). `$search` returns at most 1,000 results. | **continuation only**: opaque `pageToken`, `maxResults` ≤ 500; no offset, no anchor. | **keyset** (`(sort value, key)` after the last row) for stable forward paging; positional (`OFFSET`) available. |
| thread collapse | `collapseThreads` (§4.4.3) | none that pages: `THREAD=REFERENCES` returns the whole mailbox's trees; Gmail's IMAP has `X-GM-THRID` | none (`conversationId` exists; no collapsed listing) | `threads.list` (thread-shaped results, same paging) | window function over `thread_id` |
| total | exact on `calculateTotal: true` (omitted otherwise) | exact: `ESEARCH`/`ESORT` `COUNT` | `$count=true` → `@odata.count` (verify on messages, with and without `$filter`) | `resultSizeEstimate`: an **estimate** | exact `COUNT(*)` |
| snippets | `SearchSnippet/get` (§5): subject and preview with `<mark>` around hits, for a text filter | no hit highlighting; `PREVIEW` (RFC 8970) is a plain preview | `bodyPreview` (plain, first 255 chars); hit highlighting only in the separate Microsoft Search API | `snippet` (plain preview) | FTS5 `snippet()`/`highlight()` |
| partial body | `bodyValues` with `fetchTextBodyValues`/`fetchHTMLBodyValues` and `maxBodyValueBytes`; `isTruncated` says it was cut (§4.2) | `BODY.PEEK[section]<origin.count>` partial fetch | `body` whole (`Prefer: outlook.body-content-type`); no byte cap | `messages.get` `format=full`, parts whole; no byte cap | cached text (`message_body`) |
| blob download | `downloadUrl` template (§6.2), plain HTTP GET, streamable | `FETCH BINARY[section]` (RFC 3516) / `BODY[section]`, a literal read as it arrives | `…/attachments/{id}/$value`, `…/messages/{id}/$value`, streamable | `attachments.get`: base64url inside JSON, **whole in memory** | cached blob area |
| mailbox counts | `totalEmails unreadEmails totalThreads unreadThreads` + `myRights` (§2) | `STATUS (MESSAGES UNSEEN)`; no thread counts | `totalItemCount unreadItemCount`; no thread counts | `labels.get`: `messagesTotal messagesUnread threadsTotal threadsUnread` | from rows |
| query invalidation | `queryState` (+ `/queryChanges` if `canCalculateChanges`) | `UIDVALIDITY`, `HIGHESTMODSEQ`; `CONTEXT=SORT` `UPDATE` | none | none | store generation |

What the table says:

1. **Paging is not one model.** Two transports can jump to a position (JMAP, IMAP with the
   extensions), one can page relative to an item (JMAP), two can only continue (Graph, Gmail). A
   page type with an offset field that Graph and Gmail must fake is the leak AGENTS.md forbids:
   the host would learn which provider it has from which offsets silently fail.
2. **Relevance order exists only in the store.** No server answers "best match first" through the
   APIs above. A relevance listing is a store feature, not a query every source answers.
3. **Totals have three kinds**: exact, estimate, none. A host must be told which.
4. **Gaps sit in different adapters for each verb** (Gmail cannot stream a blob; Graph and IMAP
   cannot count threads; Graph and Gmail cannot cap a body). So each verb reports what it can do
   per adapter, and none is shaped by the provider that happens to do it best.

## The neutral query

### `MailQuery` gains order and collapse

`engine_search::MailQuery` stays the filter AST and gains two fields, both defaulting to today's
behaviour so every existing construction (`..Default::default()`) and every parsed query still
means what it did:

```rust
pub struct MailQuery {
    // …the existing filters, unchanged…
    /// How the answer is ordered. Default: `Relevance` when there is text, else `Received` desc,
    /// which is what the executor does today.
    pub order: MailOrder,
    /// One row per conversation: its newest matching message, with the thread's size.
    pub collapse_threads: bool,
}

pub enum MailOrder {
    /// Best match first. Store only (see the survey).
    Relevance,
    /// By a message property; ties broken by key so every order is total.
    By { key: MailSortKey, descending: bool },
}

#[non_exhaustive]
pub enum MailSortKey { Received, Sent, Size, From, Subject }
```

`Received` is JMAP `receivedAt`, IMAP `ARRIVAL` (`INTERNALDATE`), Graph `receivedDateTime`, Gmail
`internalDate`, and the store's `date_utc` (which falls back to `sent_at`; `search.md`). The DSL
does not gain a `sort:` operator: order is a host choice, not something typed into a search box.

### The page

```rust
/// Where a page starts.
pub enum PageStart {
    /// The top of the listing.
    Top,
    /// The item at this zero-based index. Only where `QuerySupport::positional`.
    Position(u64),
    /// Relative to an item the caller holds: `offset` 0 starts at it, 1 just after it, negative
    /// before it. Only where `QuerySupport::anchored`.
    Anchor { key: ProviderKey, offset: i32 },
    /// Straight after a page this same listing returned. Every source supports it.
    Continue(Continuation),
}

pub struct PageRequest {
    pub start: PageStart,
    pub limit: NonZeroU32,
    /// Ask for the total. Off by default: it is costly on large text searches.
    pub with_total: bool,
}

/// An opaque continuation, minted by whoever answered the page. The engine encodes the source's
/// own resume point in it: the last id and `anchorOffset` 1 for JMAP, the `(sort value, key)`
/// keyset for the store, a sorted position or UID bound for IMAP, the `@odata.nextLink` for
/// Graph, the `pageToken` for Gmail. It also carries a fingerprint of the query, so one handed
/// back with a different query is refused rather than answered wrongly.
pub struct Continuation(Box<str>);

pub struct QueryPage<T> {
    pub items: Vec<T>,
    /// The index of `items[0]` in the listing, when the source knows it.
    pub position: Option<u64>,
    pub total: Total,
    /// `None` at the end of the listing.
    pub next: Option<Continuation>,
    /// Changes when the listing changed underneath (JMAP `queryState`, store generation). A host
    /// paging by position compares it to know its page numbers have moved.
    pub listing_state: Option<ListingState>,
    pub coverage: QueryCoverage,
}

pub enum Total { Exact(u64), Estimate(u64), Unknown }
```

`Continue` is the honest common denominator: every source can do it, and on JMAP and the store it
is anchored rather than positional, so a destroy or an arrival between pages neither skips nor
repeats an item (the reason `walk.rs` anchors; `jmap.md`). `Position` and `Anchor` are offered
only where the source has them, and a caller learns that **before** asking:

```rust
pub struct QuerySupport {
    pub positional: bool,
    pub anchored: bool,
    pub total: TotalKind,        // Exact | Estimate | None
    pub collapse_threads: bool,
    pub snippets: bool,
}
```

`QuerySupport` is a function of the source **and the query**, because the answer differs within
one provider: Graph pages a `$search` differently from a `$filter`, and IMAP's answer depends on
what the server advertised. A request outside it fails with `FailureClass::InvalidState` and a
reason naming the missing *ability* (`positional paging`), never the provider. An order the source
cannot give (relevance from a server, `Size` from Gmail) is refused the same way; the engine does
not quietly substitute another order.

Emulation is allowed only where it is exact and bounded, and is then simply part of the adapter's
answer: IMAP without `SORT` can serve `Received` positionally by slicing the UID list from one
`UID SEARCH RETURN (ALL)` when UID order is arrival order, and must otherwise fetch `INTERNALDATE`
for every match. A transport that would have to read a whole mailbox to fake an ability says it
lacks it.

### Items

Both answers return the same item: the list row the store already projects (`MailListRow`, today
in `engine-store`, built by `MailListRow::project` from a normalized `Message`), plus the thread's
size when collapsed. The row moves to `engine-core` (as `MailListItem`) so a provider can return
it without depending on a store crate. A provider normalizes its summary fetch into a `Message`
with the existing normalizer and projects it the same way, so "what is a row's sender or preview"
is decided once.

## The provider layer

### Capability traits

Server-answered queries and the reads around them form a domain of their own (six methods, the
`ContactsProvider` precedent), so they are traits, not more rejecting defaults on `Provider`:

```rust
#[async_trait]
pub trait MailQueries: Send + Sync {
    /// What this source can do for `query`, from facts already known (session, capabilities).
    fn query_support(&self, query: &MailQuery) -> QuerySupport;
    async fn query_mail(&self, account: &AccountId, query: &MailQuery, page: &PageRequest)
        -> ProviderResult<QueryPage<MailListItem>>;
    /// Highlighted subject and preview for `keys` under `query`'s text.
    async fn snippets(&self, account: &AccountId, query: &MailQuery, keys: &[ProviderKey])
        -> ProviderResult<Vec<Snippet>>;
}

#[async_trait]
pub trait MailReads: Send + Sync {
    /// Every member of a conversation, oldest first.
    async fn thread(&self, account: &AccountId, thread: &ThreadId) -> ProviderResult<ThreadView>;
    /// The text/HTML body values of one message, each cut at `max_bytes` and saying so.
    async fn body_values(&self, account: &AccountId, key: &ProviderKey, request: &BodyRequest)
        -> ProviderResult<BodyValues>;
    /// A blob as a stream of bytes with its size when known. An adapter that can only fetch it
    /// whole (Gmail) streams it from memory and says so in the stream's metadata.
    async fn open_blob(&self, account: &AccountId, blob: &BlobRef) -> ProviderResult<BlobStream>;
    /// Per mailbox: total and unread messages, and threads where the transport counts them.
    async fn mailbox_counts(&self, account: &AccountId)
        -> ProviderResult<Vec<(MailboxId, MailboxCounts)>>;
}
```

`MailboxCounts { messages, unread_messages, threads: Option<u64>, unread_threads: Option<u64> }`:
the thread counts are `None` on IMAP and Graph, which do not count threads, rather than a number
derived by fetching the mailbox.

**This collides with a rule in `providers.md`**: "a new optional verb goes on `Provider` with a
rejecting default, never on its own sub-trait", because Rust will not upcast `dyn Provider` to
`dyn Sub` and a host pays for it across its signatures (measured: 97 files against 36). The agreed
direction is that the general capability-trait split comes *after* this seam. To take neither
cost now, `Provider` gains two accessors, and the traits stay free-standing:

```rust
fn mail_queries(&self) -> Option<&dyn MailQueries> { None }
fn mail_reads(&self) -> Option<&dyn MailReads> { None }
```

A host holding `Box<dyn Provider>` reaches the capability without upcasting, "not offered" is
`None` rather than a flag beside a rejecting default, and `engine-direct` can hold an adapter that
is *only* these traits. This needs the owner's OK and a rewrite of that rule in `providers.md`; it
is the first open question below.

`engine-provider` depends on `engine-core` only. It gains a dependency on `engine-search`, which
is pure (no I/O, no runtime), for `MailQuery`; the alternative is moving the query types to
`engine-core`.

### Writes and push need nothing new

The direct host's writes are the verbs `Provider` already has: `edit_mail`, `put_draft`,
`delete_draft`, `edit_mailbox`, `submit_email`. The outbox is one caller of them, not their owner.
`submit_email` takes a `HandOver` whose log is a trait (`HandOverLog`): `engine-api` passes the
outbox's, a direct host passes its own (or `Unrecorded`), and an ambiguous send comes back
classified exactly as it does to the outbox, for the host to resolve. Push is the existing `Watch`
(`JmapWatcher`, `ImapWatcher`); a direct host with many users per process is the open question on
push below, not a new trait now.

## The store answers the same query

`engine-api` gains one read:

```rust
pub async fn query_mail(&self, accounts: &[AccountId], query: &MailQuery, page: &PageRequest)
    -> Result<QueryPage<MailListItem>, ApiError>;
```

- **Order by date is a sort, not a re-sort.** With text and a date order, the FTS sources act as a
  filter (`MATCH` restricts; `bm25()` is not computed) and the executor orders by `date_utc DESC,
  key`. There is no candidate cap, so the recent-match-ranked-501st case of #83 cannot happen.
  `Relevance` keeps today's RRF fusion and pages by position over the fused list.
- **Paging is keyset.** A continuation carries the last row's `(date_utc, key)` and the next page
  starts strictly after it, so a sync committing new mail mid-paging neither repeats nor skips a
  row. `Position` is served with `OFFSET` (the store is positional too); `Anchor` reads the anchor's
  sort value and pages from there.
- **Several accounts in one query**, as #83 asks and `mail_window` already does for the unfiltered
  list. The body-FTS source joins `message_body.account` to the row's own account, not to one
  account for the whole query (`search.md`, "Executor").
- **Collapse** keeps each thread's newest matching row (a window function over `thread_id`) and
  reports the thread's member count.
- **Snippets** come from FTS5 `snippet()` over the same match.

`Engine::search_mail` stays as it is until its callers move; it then becomes a thin call of
`query_mail` with `order: Relevance`.

### Coverage: a windowed store says where its answer stops

A store holds what its sync window admitted (`store-and-sync.md`, `MailboxWindows`), so a date-
sorted answer from it is complete down to a date and unknown below it. `search-coverage.md`'s
`LocalCoverage` can say only "some objects are not local", which is true of every windowed account
and so tells a host nothing about the page it is holding. The proposal adds the date:

```rust
pub struct LocalCoverage {
    pub unsynced_objects: bool,
    pub unindexed_content: bool,
    /// When `unsynced_objects` is due to a date window: mail dated at or after this is all
    /// local. Rolled up across scopes as the latest floor.
    pub complete_since: Option<UtcDateTime>,
}
```

The floor is each scope's window as its cursor records it (`engine_sync::window_cursor`). For a
newest-first listing, every page whose last row is dated at or after the floor is complete as a
page; the first page that crosses it is where a host says "older mail is not on this device" and,
if it wants, asks the server for exactly the rest: the same `MailQuery` with `before` set to the
floor, through `MailQueries`. Because the split is by date, the two answers concatenate without a
merge. The temporal axis keeps its meaning (recurrence only). For a relevance listing the floor
cannot bound anything, and `unsynced_objects` stays the whole answer.

This is also where the current executor's simplification ends: it reports every scope as locally
complete (`search-coverage.md`, "Provenance"). Shipping `query_mail` without wiring the floor would
present a windowed answer as complete, which is the failure #83 is about.

A server answer reports `RemoteCoverage::Augmented { exhaustive }`, with `exhaustive: false` where
the server capped it (Graph's 1,000 `$search` results). Gmail's total is reported as `Estimate`,
never as `Exact`.

## `engine-direct`

A crate in the engine repository, product-neutral, beside `engine-api`. It is what a store-less
host depends on.

- **Composes:** `engine-core` types; `engine-search` (`MailQuery`, `QueryPage`); `engine-provider`
  (the traits above and the write verbs); `engine-mime` and `engine-html` for the body a host
  renders (the one sanitiser, `html.md`); the adapters behind Cargo features, JMAP first.
- **Does not contain:** a store, an outbox, a sync loop, a cursor, or SQLite. It never writes to
  disk.
- **Per-request credentials:** every call takes the account's `CredentialSource` (being designed
  separately; this note only depends on it). Today `JmapConfig` binds `Credentials` into the
  transport at connect. The adapter instead asks the source for a credential per HTTP request, so a
  refreshed token is used at once and one process serves many users. Connecting fetches the JMAP
  session document; `engine-direct` caches sessions keyed by the authenticated principal (never by
  token, which rotates) and refetches when a response's `sessionState` differs.
- **Surface:** `query_mail`, `snippets`, `thread`, `body` (body values, then `engine-html`'s
  reading document), `open_blob`, `mailbox_counts`, and the writes `edit_mail`, `put_draft`,
  `delete_draft`, `edit_mailbox`, `submit` (with the host's `HandOverLog`), each returning the
  provider verb's own receipt. Each read checks `QuerySupport` or the trait accessor first and
  answers a missing ability with its class, never a provider name.
- **Logging:** none of content, as everywhere in the engine. A continuation handed to a browser
  carries ids and a server's next-link, never a credential.

## What `provider-jmap` makes public

Public, because a direct host still needs JMAP the facade does not wrap (Sieve, vacation response,
a server's extension capability), and because it is what the facade itself is built from:

| today (`pub(crate)`) | becomes |
|---|---|
| `request::Request` (`new(using)`, `invoke`, `to_json`) | public builder; `invoke` returns a `CallId` newtype instead of a `String` |
| `request::Response` (`result`, `into_result`) | public, keyed by `CallId`; method errors stay typed (`JmapError::Method`) |
| `result_ref`, `with_back_reference` | one public `ResultRef { of: CallId, name, path }`, set on an invocation's arguments |
| `request::capability` constants | public |
| `JmapClient::execute`, `upload` | public |
| `JmapClient::download` (whole `Vec<u8>`) | public, plus a streaming variant the blob reads use |
| `blob::download_url` (percent-encoding every substitution) | public |
| `Session::{download_url, upload_url, event_source_url, mail_account_id, …}` | public, returning a JMAP account id newtype, not `&str`, so it cannot be mixed with the engine's `AccountId` |

Kept internal: `Walk` and the sync passes, the normalizers (reached through the neutral verbs),
`Executor` (the test seam). A direct host's own JMAP client then reduces to calls of
`engine-direct` for list, thread, body, blob and counts, and of the public builder for whatever
the facade does not cover; its private copy of the request envelope, session parsing, back-references and download
template goes.

## Phasing

1. **Store-answered date-sorted search (#83).** `MailOrder`, `PageRequest`, `QueryPage`, `Total`,
   `Continuation` in `engine-search`; `MailListItem` moved to `engine-core`; `Engine::query_mail`
   over several accounts with keyset paging; `complete_since` wired from the recorded windows.
   Collapse may follow in a second PR. No provider change.
2. **JMAP-answered sorted, paged mailbox list.** The `provider-jmap` primitives above become
   public; `MailQueries` (accessor on `Provider`) implemented for JMAP: one request of
   `Email/query` (`inMailbox`, `receivedAt` descending, `collapseThreads`, `position`/`anchor`,
   `calculateTotal` on request) with back-referenced `Email/get` and `Thread/get`. Live-tested
   (below).
3. **`engine-direct`** once `CredentialSource` lands: `MailReads` for JMAP (thread, body values,
   blob stream, counts, snippets) and the direct writes. A direct host's private JMAP client can
   be deleted.
4. **The other adapters**, each with its live test: Graph and Gmail as continuation-only sources,
   IMAP per advertised extension.
5. The capability-trait split, then push for many users.

Phases 1 and 2 are independent and can land in either order.

## Live test plan (Stalwart harness)

The offline fakes answer canned bytes whatever is sent, so every request shape below is proven
against the harness (`scripts/ci/stalwart-live.sh`), its response captured as a scrubbed fixture
(reserved domains only), and each check shown to fail with its fix reverted.

1. **Pages concatenate to the listing.** Seed a mailbox larger than three pages with distinct
   `receivedAt`; page with `Continue` and with `Position`; both concatenations equal one request for
   the whole listing, in order.
2. **Anchored continuation survives change; position does not.** Destroy an item above the next
   page between two requests: `Continue` loses nothing, `Position` skips one (the
   `live_snapshot_walk` pattern, so the test can fail).
3. **`anchorNotFound`.** Destroy the continuation's anchor; the adapter recovers as `walk.rs` does,
   with no item repeated.
4. **Collapse, both directions.** `collapseThreads: true` gives one row per thread and its total
   equals the mailbox's `totalThreads`; the same query with `false` gives more rows and equals
   `totalEmails`. Without the `false` arm, an adapter that never sent the flag would pass.
5. **Total only when asked.** With `calculateTotal` the page has `Exact`; without, `Unknown`.
6. **Server-capped `limit`.** Ask above the server's cap; the page is the size the server says and
   the continuation still reaches the end.
7. **Stalwart's late placement.** A message stored just before the first page is listed last and
   then moved (`jmap.md`); record what a page sees, and add an entry to `spec-deviations.md` if a
   page can show it out of order.
8. **Snippets, both directions.** `SearchSnippet/get` with a text filter returns `<mark>`; without
   text it returns none.
9. **Body cap, both directions.** `maxBodyValueBytes` below the body's size gives `isTruncated`;
   above it, the whole value.
10. **Blob stream.** A seeded attachment streamed through `open_blob` is byte-equal to the seed.
11. **Counts agree.** `mailbox_counts` equals the totals of the matching queries.
12. **Store parity.** Sync the same mailbox and run the same `MailQuery` through `Engine::query_mail`:
    same keys in the same order, and with a sync window set, `complete_since` equals it and the
    page crossing it is the first one marked incomplete.
13. **IMAP on the same harness.** Record what Stalwart's IMAP advertises (`SORT`, `ESORT`,
    `CONTEXT=SORT`, `THREAD`, `PARTIAL`, `PREVIEW`) before designing the IMAP adapter's paging.

Graph and Gmail use their test accounts when phase 4 starts: `$search` with `$orderby` and with
`$skip` (send both and record the refusal), `$count` on messages, Gmail's order with and without
`q`, and how far `resultSizeEstimate` is from the true count.

## Decisions (2026-11-23)

- **Capabilities are reached through accessors on `Provider`.** A new capability is its own trait, reached through a `Provider` method that defaults to `None` (for example `fn mail_rules(&self) -> Option<&dyn MailRules> { None }`). It is additive, works through `Box<dyn Provider>`, and is the first step of the capability-trait split. It supersedes the `providers.md` rule that rejects sub-traits; that doc changes with the first slice that adds one. This settles open question 1.

## Open questions

1. **Trait shape.** Free-standing capability traits reached through `Option<&dyn …>` accessors on
   `Provider`, against the current "rejecting default, never a sub-trait" rule. Needs the owner's
   OK, and is a breaking change to the provider traits.
2. **`MailListRow` to `engine-core`.** A public type moves crate (re-exported from `engine-store`
   for a release, or not).
3. **Extend `MailQuery` or wrap it.** Extending keeps one type, as agreed; a wrapper
   (`MailListing { query, order, collapse }`) keeps the filter AST pure. This note extends.
4. **Continuations across an untrusted boundary.** A direct host may hand them to a browser.
   Validate on return only (parse, query fingerprint, same-origin next-link, as `same_origin`
   already does), or also seal them with a host key in `engine-direct`?
5. **Filters the hosts need that `MailQuery` lacks:** not-keyword (unread only), not-in-mailbox
   (exclude Trash and Junk), and a header match (`Message-ID`, for a transfer host's spot checks).
6. **Push for many users per process.** One EventSource per user, or RFC 8620 §7.2
   `PushSubscription` (and Graph subscriptions, Gmail `watch`), which need an endpoint the host
   owns.
7. **Server-side relevance.** None of the four APIs above orders by relevance; Graph's separate
   Search API ranks and highlights. Is relevance store-only for good?
8. **Session cache identity.** How `CredentialSource` names the principal a cached session belongs
   to.
