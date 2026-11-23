# Design: a sorted, paged mail query, and the store-less facade `engine-direct`

Status: **accepted**. The public `provider-jmap` client is implemented ("What `provider-jmap`
makes public"); the rest is not. The owner's decisions are listed at the end
("Decisions (2026-11-23)") and written into the body; no question is left open. The slice that
ships each part updates `search.md`,
`search-coverage.md`, `engine-api.md`, `providers.md` and `jmap.md` in the same change
(AGENTS.md, "Documentation Currency"). The host kinds and principles cited are those of
`north-star.md` (#378); the credential side is the companion note `credential-source.md` (#381).

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

The principle this note follows (`north-star.md`, principle 1): **each protocol is known once.**
The engine answers the query from the store or from the server through one neutral query type and
one page type, and the JMAP primitives a host still needs become public, so a host-side copy can
be deleted.

## Requirements by host

| need | sync host | direct host | transfer host |
|---|---|---|---|
| answered from | store (server for what the window lacks) | server only | server only |
| sort | date (received) desc; relevance on request | received desc; others where offered | received or none |
| filter | text + DSL operators, one mailbox or all, several accounts | one mailbox or all but Trash/Junk; unread only; text search | mailbox; `Message-ID`/date for spot checks |
| paging | "show more" (forward); stable while sync inserts | jump to page *n* (position), open-around-a-message (anchor), forward | forward, whole listing |
| total | exact count of matches | exact where offered, else estimate or none | exact (verify) |
| collapse threads | conversation list | yes | no |
| snippets / highlight | yes (from the store's FTS) | yes (server) | no |
| thread view | store (`mail_on_threads`) | server | no |
| body | cached (`message_body`) | partial: text/HTML values capped in bytes | raw source (exists) |
| blobs | cached | streamed, never whole in memory | streamed raw source |
| mailbox counts | from the store | total/unread, messages and threads | totals per mailbox (verify) |
| push | `Watch` (exists) | one event stream per active user session, many per process | no |
| credentials | bound at connect (today) | per request (`CredentialSource`) | per run |
| writes | outbox (`engine-api`) | direct, host owns retries | import seam (`Provider::mail_importer`) |
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
   APIs above. A relevance listing stays a store capability (decision 7); servers answer the
   other orders.
3. **Totals have three kinds**: exact, estimate, none. A host must be told which.
4. **Gaps sit in different adapters for each verb** (Gmail cannot stream a blob; Graph and IMAP
   cannot count threads; Graph and Gmail cannot cap a body). So each verb reports what it can do
   per adapter, and none is shaped by the provider that happens to do it best.

## The neutral query

### `MailQuery` gains order, collapse and three filters

`engine_search::MailQuery` is extended, not wrapped (decision 3): it stays the one filter AST and
gains the fields below, each defaulting to today's behaviour so every existing construction
(`..Default::default()`) and every parsed query still means what it did. It stays in
`engine-search`, the pure, I/O-free query vocabulary crate that both answerers (the store and the
providers) depend on; principle 4 asks for one shared definition, and this is it.

```rust
pub struct MailQuery {
    // …the existing filters, unchanged…
    /// Messages with none of these keywords: `$seen` here lists unread mail only.
    pub not_keyword: Vec<String>,
    /// Messages in none of these mailboxes: an "all mail" listing without Trash and Junk.
    pub not_mailbox: Vec<String>,
    /// Messages whose `Message-ID` is one of these, for a transfer host's spot checks.
    pub message_id: Vec<MessageIdHeader>,
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
does not gain a `sort:` operator, nor operators for the three new filters: order, an unread
toggle, a folder set and a spot check are host choices, not something typed into a search box.

Every source has a way to express the three filters (decision 5); the two Graph forms marked
"verify" are settled by the live tests below. Several values combine as JMAP's `AND` operator
does.

| filter | JMAP (RFC 8621 §4.4.1) | IMAP | Graph | Gmail API | Store |
|---|---|---|---|---|---|
| `not_keyword` | `notKeyword` | `UNKEYWORD`; `UNSEEN` for `$seen` | `isRead eq false` for `$seen`, `flag/flagStatus` for `$flagged`; other keywords are categories, `not(categories/any(…))` (verify) | `is:unread` for `$seen`, `-is:starred` for `$flagged`, `-label:` otherwise | anti-join on the keyword rows |
| `not_mailbox` | `inMailboxOtherThan` | a search runs in one selected mailbox, so an excluded mailbox is not selected | `parentFolderId ne` (verify) | `-in:`/`-label:`; Trash and Spam are left out unless `includeSpamTrash` | anti-join on membership |
| `message_id` | `header: ["Message-ID", value]` | `HEADER Message-ID` | `internetMessageId eq` | `rfc822msgid:` | the indexed `Message-ID` |

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
/// back with a different query is refused rather than answered wrongly. It is not sealed; see
/// "Continuations across an untrusted boundary" below.
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
cannot give (relevance from a server, `Size` from Gmail), or a filter it cannot apply, is refused
the same way; the engine does not quietly substitute another order or drop the filter.

Emulation is allowed only where it is exact and bounded, and is then simply part of the adapter's
answer: IMAP without `SORT` can serve `Received` positionally by slicing the UID list from one
`UID SEARCH RETURN (ALL)` when UID order is arrival order, and must otherwise fetch `INTERNALDATE`
for every match. A transport that would have to read a whole mailbox to fake an ability says it
lacks it.

### Continuations across an untrusted boundary

A direct host may hand a continuation to a browser and get it back on the next request. The engine
**validates it on return** (decision 4): it must parse, its query fingerprint must match the
query it comes back with, and a next-link it carries must be same-origin with the account's base
(the existing `same_origin` check), so a forged one cannot send the user's credential elsewhere.
The engine does not seal continuations with a host key: the server's own authorisation is the
real boundary, and a tampered continuation reaches only what that user may already see. A host
that wants sealing adds it on its side, around the opaque string.

### Items

Both answers return the same item: the list row the store already projects (`MailListRow`, today
in `engine-store`, built by `MailListRow::project` from a normalized `Message`), plus the thread's
size when collapsed. The row moves to `engine-core` as `MailListItem` (decision 2), so the store
and the server return one row type and a provider can return it without depending on a store
crate. `engine-store` keeps `MailListRow` as a deprecated re-export for one host pin bump, then
drops it. A provider normalizes its summary fetch into a `Message` with the existing normalizer
and projects it the same way, so "what is a row's sender or preview" is decided once.

## The provider layer

### Capability traits

Server-answered queries and the reads around them form a domain of their own (seven methods), so
they are capability traits, reached the way `MailImporter` is (#375), not more rejecting defaults
on `Provider` (decision 1; `north-star.md`, principle 3):

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

`Provider` gains two accessors, as it gained `mail_importer` in #375, each defaulting to `None`
and delegated by the `Box<dyn Provider>` impl:

```rust
fn mail_queries(&self) -> Option<&dyn MailQueries> { None }
fn mail_reads(&self) -> Option<&dyn MailReads> { None }
```

A host holding `Box<dyn Provider>` reaches the capability without upcasting, so the cost
`providers.md` measured for a sub-trait (97 changed files against 36) does not arise; "not
offered" is `None` rather than a flag beside a rejecting default; and `engine-direct` can hold an
adapter that is *only* these traits. The change is additive, so it breaks no adapter and no host.
`north-star.md` (#378) records this as how new capabilities are reached and as the first step of
the capability-trait split, which lands later as one planned migration, after this seam and the
credential source. `providers.md` describes the pattern, and #386 replaces its older
"rejecting default, never a sub-trait" bullet with it.

`engine-provider` depends on `engine-core` only. It gains a dependency on `engine-search`, which
is pure (no I/O, no runtime), for `MailQuery` and the page types, which stay there.

### Writes and push need nothing new

The direct host's writes are the verbs `Provider` already has: `edit_mail`, `put_draft`,
`delete_draft`, `edit_mailbox`, `submit_email`. The outbox is one caller of them, not their owner.
`submit_email` takes a `HandOver` whose log is a trait (`HandOverLog`): `engine-api` passes the
outbox's, a direct host passes its own (or `Unrecorded`), and an ambiguous send comes back
classified exactly as it does to the outbox, for the host to resolve. An upload into a mailbox is
the import seam (`Provider::mail_importer`, #375), with the metadata every stored message carries
(`engine_core::mail::MessageMetadata`, #383).

Push is the existing `Watch` (`JmapWatcher`, `ImapWatcher`). A direct host with many users per
process starts with one event stream per active user session (JMAP EventSource, RFC 8620 §7.3),
opened while that user is signed in and closed with the session (decision 6). It needs no inbound
endpoint. Webhook-style subscriptions (JMAP `PushSubscription`, §7.2; Graph subscriptions; Gmail
`watch`) come later, as an option for a host that owns a reachable endpoint; they are not a new
trait now.

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
and so tells a host nothing about the page it is holding. This design adds the date:

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
- **Per-request credentials:** every call takes the account's `CredentialSource`, designed in the
  companion note `credential-source.md` (#381; this note only depends on it). Today `JmapConfig`
  binds `Credentials` into the transport at connect. The adapter instead asks the source for a
  credential per HTTP request, so a refreshed token is used at once and one process serves many
  users. Connecting fetches the JMAP session document; `engine-direct` caches sessions keyed by
  the source's `CredentialSource::subject()`, a `CredentialSubject` newtype (decision 8; never by
  token, which rotates, nor by origin alone), and refetches when a response's `sessionState`
  differs.
- **Surface:** `query_mail`, `snippets`, `thread`, `body` (body values, then `engine-html`'s
  reading document), `open_blob`, `mailbox_counts`, and the writes `edit_mail`, `put_draft`,
  `delete_draft`, `edit_mailbox`, `submit` (with the host's `HandOverLog`), and an upload into a
  mailbox through `mail_importer`, each returning the provider verb's own receipt. Each read
  checks `QuerySupport` or the trait accessor first and answers a missing ability with its class,
  never a provider name.
- **Logging:** none of content, as everywhere in the engine. A continuation handed to a browser
  carries ids and a server's next-link, never a credential, and is validated when it comes back
  ("Continuations across an untrusted boundary").

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

**Shipped** as the first part of phase 2, as the table says, plus `blob::upload_url` beside
`download_url` (the one encoder for both templates) and one rule the public methods add: a blob
request goes only to the origin of the session's own template (`jmap.md`, "The public client").

Kept internal: `Walk` and the sync passes, the normalizers (reached through the neutral verbs),
`Executor` (the test seam). A direct host's own JMAP client then reduces to calls of
`engine-direct` for list, thread, body, blob and counts, and of the public builder for whatever
the facade does not cover; its private copy of the request envelope, session parsing, back-references and download
template goes.

## Phasing

1. **Store-answered date-sorted search (#83).** `MailOrder`, the three filters, `PageRequest`,
   `QueryPage`, `Total`, `Continuation` in `engine-search`; `MailListItem` moved to `engine-core`
   with the deprecated `MailListRow` re-export; `Engine::query_mail` over several accounts with
   keyset paging; `complete_since` wired from the recorded windows. Collapse may follow in a
   second PR. No provider change.
2. **JMAP-answered sorted, paged mailbox list.** The `provider-jmap` primitives above become
   public; `MailQueries` (accessor on `Provider`) implemented for JMAP: one request of
   `Email/query` (`inMailbox`, `receivedAt` descending, `collapseThreads`, `position`/`anchor`,
   `calculateTotal` on request) with back-referenced `Email/get` and `Thread/get`. Live-tested
   (below).
3. **`engine-direct`** once `CredentialSource` lands (`credential-source.md`): `MailReads` for
   JMAP (thread, body values, blob stream, counts), `MailQueries::snippets`, the direct writes,
   and one EventSource per active user session for push. A direct host's private JMAP client can
   be deleted.
4. **The other adapters**, each with its live test: Graph and Gmail as continuation-only sources,
   IMAP per advertised extension.
5. Webhook-style push (`PushSubscription`, Graph subscriptions, Gmail `watch`) for hosts that own
   an endpoint. The general capability-trait split is its own planned migration (`north-star.md`);
   the accessors above already have its shape, so it changes nothing here.

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
14. **Filters, both directions.** `notKeyword: "$seen"`, `inMailboxOtherThan` (Trash and Junk)
    and `header: ["Message-ID", …]` each leave out what they should, and the same query without
    the filter returns it; without that arm an adapter that never sent the filter would pass.

### Settled by live tests

The claims marked "verify" above are settled by sending the request, not by documentation. Each
result is recorded in the adapter's guidance doc, and a refusal is recorded only after the request
that asks for the ability was sent and its answer captured:

- **Graph**, on its test account when phase 4 starts: `$search` with `$orderby`, and `$search`
  with `$skip` (both community-reported as refused); `$count=true` on messages, with and without
  `$filter`; `parentFolderId ne` and `not(categories/any(…))` in `$filter`.
- **Gmail**, on its test account when phase 4 starts: the order of `messages.list` and
  `threads.list`, with and without `q`; how far `resultSizeEstimate` is from the true count.
- **Stalwart IMAP**, on the harness before the IMAP adapter's paging is designed: whether it
  advertises and answers `SORT`, `ESORT`, `CONTEXT=SORT` and `PARTIAL` (item 13).

## Decisions (2026-11-23)

The owner approved these. Each is written into the body above; none is open.

1. **Capabilities are reached through accessors on `Provider`.** A new capability is its own
   trait, reached through a `Provider` method that defaults to `None` and is delegated by the
   `Box<dyn Provider>` impl: here `mail_queries()` and `mail_reads()`, as `mail_importer()` is
   (merged in #375) and as `north-star.md` records for every new capability (#378). It is
   additive, so not breaking, works through `Box<dyn Provider>`, and is the first step of the
   capability-trait split. It supersedes the `providers.md` rule against sub-traits, which #386
   rewrites.
2. **`MailListRow` moves from `engine-store` to `engine-core` as `MailListItem`**, so the store
   and the server return one row type. `engine-store` keeps a deprecated `MailListRow` re-export
   for one host pin bump.
3. **`MailQuery` is extended** with `order` and `collapse_threads`, not wrapped. It stays in
   `engine-search`, the pure query vocabulary crate both answerers depend on.
4. **Continuations crossing an untrusted boundary are validated on return**: they parse, their
   query fingerprint matches, and a carried next-link is same-origin. The engine does not seal
   them with a host key; the server's own authorisation is the real boundary, and a tampered
   continuation reaches only what that user may already see. A host may add sealing on its side.
5. **`MailQuery` gains the filters hosts need**: `not_keyword` (unread only), `not_mailbox`
   (exclude Trash and Junk) and `message_id`, a header match on `Message-ID` for a transfer host's
   spot checks.
6. **Push for a direct host with many users starts with one event stream per active user
   session** (JMAP EventSource), which needs no inbound endpoint. Webhook-style subscriptions
   (JMAP `PushSubscription`, Graph subscriptions, Gmail `watch`) come later, as an option for
   hosts that own an endpoint.
7. **Relevance ordering stays a store capability.** Servers answer the other orders; a request
   for relevance from a server is refused by `QuerySupport`.
8. **A cached session's identity is the `CredentialSubject`** returned by
   `CredentialSource::subject()` (`credential-source.md`, #381).
