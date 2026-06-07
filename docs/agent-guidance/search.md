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
| `before: after:` (mail) | `mail_index.date_utc` |
| `before: after:` (calendar) | `event_occurrence` time range |
| `subject:`/`location:` + free text | FTS5 external-content (`fts_doc` → `fts_index`) |
| `has_attachment:`/`has_conference:` | `mail_index`/`event_index` scalar |
| `rsvp:` | `event_index.my_partstat` |

Projection decisions (settled with the user):

- **`mail_index.date_utc` = `received_at` ?? `sent_at`** (JMAP `Email/query`
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
  trimmed + lowercased) on **both** the storage and query sides, so a query
  address matches the stored one. Matching is exact-normalized (substring is a
  future refinement).
- **`OccurrenceRow`s are not projected** — expanding recurrence to UTC instants
  needs tzdata and is a separate step (`calendar-semantics.md`); until it lands,
  calendar `before:`/`after:` matches only occurrences that were materialized
  some other way.

## FTS5

External-content FTS5 (`fts_index`) over a reshaped `fts_doc` carrying a stable
integer rowid and typed `subject`/`body`/`location` columns; triggers keep the
index in sync with `fts_doc`. Tokenizer is **`porter unicode61`** (CJK/trigram
later), ranked by **`bm25()`** (smaller = better; the executor orders ascending).
The projection's field-tagged text folds onto the three columns: `subject` and
`location` map by name, every other field (body, preview, future attachment text)
folds into `body`, so unscoped free text still matches it. One shared index serves
both domains; the executor restricts by `scope_key`.

## Executor

`store-sqlite::search_ops` compiles a query to:

1. a **structured-filter predicate** — `EXISTS` on the junctions plus scalar/date
   conditions, **AND** across filters and **OR** within a repeated one (an `IN`
   list), correlated to the base index table;
2. an optional **FTS5 `MATCH`** (every term a quoted phrase so user input cannot
   inject FTS operators; scoped terms carry a column filter), ranked by `bm25()`.

Ranked candidate lists fuse with **RRF** (`engine_search::fuse`). Today FTS is the
only ranked source, so single-list fusion reproduces the bm25 order; a query with
no text falls back to a deterministic order (mail by date desc, calendar by key).
The result is ranked provider keys (`SearchResults`) plus assembled coverage.

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
- **Coverage gap detection** and **occurrence/horizon expansion** (the latter is
  the remaining step-2 item: recurrence fixtures + ingestion CLI).
- **Substring/prefix address matching** (currently exact-normalized).
