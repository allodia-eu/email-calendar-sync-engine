# Search Architecture

How a query becomes a ranked answer. `north-star.md` states the Search Contract;
this document is authoritative for the layering, the DSL, the projection→table
mapping, and the executor. `search-coverage.md` is authoritative for the coverage
model. Read both before touching search.

## Two halves

Search splits into a **store-agnostic** half and a **per-store** half so a second
backend (Postgres) reuses the portable pieces and only re-implements execution.

- **`engine-search`** (store-agnostic, no I/O, no SQL): the query **AST**
  (`query.rs`), the textual **DSL parser** (`parse.rs`), **reciprocal-rank fusion**
  (`rrf.rs`), **coverage assembly** (`coverage.rs`, onto
  `engine_core::coverage::SearchCoverage`), and the **result types**
  (`result.rs`).
- **`engine-core::search_index`** (store-agnostic projection): pure
  `project_message`/`project_event` turn a normalized object into its derived
  rows. This is the "compute" the store must never do.
- **per-store executor** (`store-sqlite::search_ops`): compiles the AST to native
  SQL — indexed structured filters + FTS5 `MATCH`/`bm25()` — fuses with RRF, and
  assembles coverage. `SqliteStore::search_mail`/`search_calendar` are the entry
  points; search is **per account** (pass that account's scopes).

## The DSL

Per-domain operators (`north-star.md` Search Contract), parsed into the AST:

- **mail:** `from to cc subject has_attachment before after mailbox label keyword`
- **calendar:** `calendar attendee organizer rsvp location has_conference before
  after`

Rules (`parse.rs`): only a **known keyword** before a colon is an operator —
anything else (`http://x`, `3:1`, a typo) is free text, so there is no
"unknown-operator" failure. Quoting binds spaces (`subject:"q report"`);
`before:`/`after:` take `YYYY-MM-DD`; `has_*:` take an explicit bool; `rsvp:` maps
to `ParticipationStatus` (open enum, unknown values preserved). `subject:`/
`location:` are full-text scopes, not structured filters — they execute through
FTS, matching the schema below.

## Projection → schema mapping

`project_message`/`project_event` produce the derived rows; the store writes them
into the V2 schema (`store-sqlite::schema::V2`). The mapping:

| Filter | Table / column |
|---|---|
| `from: to: cc:` / `attendee: organizer:` | `mail_address(field, addr)` / `event_participant(role, addr)` junctions |
| `mailbox: label: keyword:` / `calendar:` | `membership(kind, value)` junction |
| `before: after:` (mail) | `message.date_utc` |
| `before: after:` (calendar) | `event_occurrence` time range |
| `subject:`/`location:` + free text | FTS5 external-content (`fts_doc` → `fts_index`) |
| `has_attachment:`/`has_conference:` | `message`/`event_index` scalar |
| `rsvp:` | `event_index.my_partstat` |

Projection decisions (settled with the user):

- **`message.date_utc` = `received_at` ?? `sent_at`** (JMAP `Email/query`
  convention); `NULL` excludes a message from date filters.
- **Mailbox and label are one membership kind.** The model unifies folders and
  labels (`modeling.md`), so projection emits `kind = mailbox`, and the executor
  treats `mailbox:` and `label:` as synonyms over it. `membership.value` is the
  collection **id** (not a display name); name→id resolution is a host concern.
  `keyword:` membership values are lowercased to match the canonical keyword form.
- **`rsvp:` is "how *I* responded".** `project_event` takes the account's own
  addresses (`OwnerAddresses`) and records the matching participant's status as
  `event_index.my_partstat`. Identity is per account, so a single instance with
  several accounts (including several of the same provider) resolves "me"
  independently for each.
- **Addresses are normalized** (`engine_core::search_index::normalize_addr`,
  trimmed + lowercased) on **both** the storage and query sides, so a scoped
  `from:`/`to:`/`cc:` query address matches the stored one. That *structured*
  matching is exact-normalized (full substring on the operator is a future
  refinement). Separately, the address text is also folded into the FTS `body`
  (see "FTS5"), so an *unscoped* search-box term prefix-matches an address.
- **`OccurrenceRow`s are not projected by `search_index`** — expanding recurrence
  to UTC instants needs bundled tzdata, so it lives in `engine_recurrence::expand`
  (`calendar-semantics.md`), which the ingest/maintenance path runs before the store
  call. The executor consumes the materialized `event_occurrence` rows unchanged;
  calendar `before:`/`after:` matches occurrences within the host horizon.

## FTS5

External-content FTS5 (`fts_index`) over a reshaped `fts_doc` carrying a stable
integer rowid and typed `subject`/`body`/`location` columns; triggers keep the
index in sync with `fts_doc`. Tokenizer is chosen at creation — **`porter
unicode61`** by default, `trigram` opt-in (next section) — ranked by **`bm25()`**
(smaller = better; the executor orders ascending).
The projection's field-tagged text folds onto the three columns: `subject` and
`location` map by name, every other field (body, preview, future attachment text)
folds into `body`, so unscoped free text still matches it. One shared index serves
both domains; the executor restricts by `scope_key`.

**Sender/recipient addresses are part of the indexed text.** `project_message`
folds each `from`/`to`/`cc` address's email **and** display name into the mail
`body` field (alongside preview/reply text), so a bare search-box term matches an
address — essential because a metadata-tier message (e.g. IMAP) has an empty body,
and the only searchable identity is the address. The `unicode61` tokenizer splits
`info@allodia.eu` into `info`/`allodia`/`eu` and case-folds, so a typed `allodia`
matches. This is in addition to the structured `mail_address` rows, which still
back the exact, scoped `from:`/`to:`/`cc:` filters (the fold does not replace them).

### Tokenizer: chosen at creation, never changed

Both FTS tables (`fts_index`, `message_body_fts`) are created with one tokenizer,
picked through `store-sqlite::OpenOptions { fts_tokenizer }` — a **creation-time
choice** exposed to hosts as `Engine::open_with` / `open_in_memory_with`
(`engine-api.md`). The default stays **`porter unicode61`**; `FtsTokenizer::Trigram`
(FTS5 `trigram`) is the CJK option:

- **Fresh databases only.** The option shapes a database the open itself creates.
  An existing database carries the tokenizer it was made with: it is recorded once
  into `meta.fts_tokenizer` (the `FtsTokenizer::sql()` string, read back by
  `from_meta` on every later open), and an open that requests a different one is
  refused with a `StoreError::Backend` naming both values and the recovery —
  recreate the database and re-sync. A database created before the option existed
  reads back as the default.
- **No in-place re-tokenization, by design.** The token stream lives inside the
  FTS index, not in any recoverable source table, so the engine never rebuilds it
  over live data. Changing the tokenizer means recreating the database; its
  contents re-derive by re-sync (sync is the derivation, the store is the cache).
- **Trigram semantics: ≥3-character substring matching.** A mid-string query like
  `会议纪` matches `会议纪要` — where `porter unicode61` cannot match mid-CJK at
  all (a Han run is one token to it). The floor binds the executor's own query
  form: every term is a `"term"*` phrase-prefix (see "Executor"), and under
  trigram **a query shorter than 3 characters matches nothing** — the index's
  entries are 3-grams, so 1–2-character search-as-you-type has no hits. These
  behaviors are pinned by acceptance tests in `store-sqlite` (`tests.rs`).

| | `porter unicode61` (default) | `trigram` |
|---|---|---|
| Latin search-as-you-type (`allo` → `allodia`) | prefix match | substring match |
| English stemming (`porter`) | yes | no — raw 3-character substrings |
| CJK mid-string (`会议纪` → `会议纪要`) | no | yes |
| 1–2-character query (`al`) | hits (prefix form) | **no hits** |

A CJK-facing host picks `Trigram` at first launch — before the database exists —
and gates its own suggestion/autocomplete surface on ≥3 typed characters.

## Executor

`store-sqlite::search_ops` compiles a query to:

1. a **structured-filter predicate** — `EXISTS` on the junctions plus scalar/date
   conditions, **AND** across filters and **OR** within a repeated one (an `IN`
   list), correlated to the base index table;
2. an optional **FTS5 `MATCH`** — every term is a quoted-phrase **prefix** query
   (`"term"*`): the quoting keeps user input from injecting FTS operators, and the
   trailing `*` makes search-as-you-type match partial words (a typed `allo`
   matches `allodia`). Scoped terms carry a column filter (`subject:"allo"*`).
   Ranked by `bm25()`. Under the `trigram` tokenizer the same form is subject to
   its ≥3-character floor: `"会议纪"*` matches mid-string, `"会议"*` matches
   nothing (see "Tokenizer" above).

Ranked candidate lists fuse with **RRF** (`engine_search::fuse`). For **mail**, free
text matches **two** FTS sources fused together: the scope-derived `fts_index`
(subject + folded address text) and the lease-free **`message_body_fts`** over the
on-demand-fetched body text (`store-and-sync.md`). The body source is mail-only,
matches the *unscoped* terms (its single `plain` column has no `subject:`/`location:`
qualifier), and is joined to `message` (live, in-scope keys only) and
`message_body.account` (IMAP keys can collide across accounts) — so a body row for a
deleted message or another account never surfaces. A purely `subject:`-scoped query
does not search the body. Calendar has the single FTS source. A query with no text
falls back to a deterministic order (mail by date desc, calendar by key). The result
is ranked provider keys (`SearchResults`) plus assembled coverage; the fused list is
truncated to the limit.

## Coverage

`engine_search::assemble` remote-compensates each scope then conservatively rolls
up (`search-coverage.md`). The v1 executor reports each searched scope as
**locally complete**: real gap detection (unsynced/unindexed objects from partial
sync, recurrence-horizon bounds, remote augmentation) arrives with sync-state and
occurrence-horizon integration. The assembly path is wired so those facts compose
in without changing callers.

## Deferred (wired seams, not yet implemented)

- **Vector KNN.** The `embedding` table exists; the `sqlite-vec` extension
  (`vec0` KNN, per-platform bundling, `load_extension`) is a later **Cargo-feature
  -gated** source that joins the same RRF fusion. "FTS works before vectors"
  (`north-star.md`).
- **Coverage gap detection** and **on-demand beyond-horizon expansion** with
  `TemporalCoverage::Bounded` reporting. Occurrence materialization within the
  horizon now exists (`engine-recurrence`, ingest/maintenance via `engine-cli`); the
  read-path expansion past the horizon and the temporal-coverage reporting are the
  follow-up (`search-coverage.md`).
- **Substring/prefix matching on the *structured* `from:`/`to:`/`cc:` operators**
  (those junction lookups are still exact-normalized). Unscoped free-text search
  already prefix-matches addresses through the FTS `body` fold (see "FTS5"); this
  remaining item is the scoped-operator form.
