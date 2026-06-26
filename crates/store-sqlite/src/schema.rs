//! The durable schema, as versioned DDL.
//!
//! Each `const` here is one migration step's SQL; [`crate::migrations`] runs them
//! in order keyed on `PRAGMA user_version`. To evolve the schema, add a new `Vn`
//! const and append it to the migration list — never edit a shipped step, since
//! existing databases have already applied it.
//!
//! Five tables back the store contract. `sync_scope` holds the per-scope fencing
//! generation, lease expiry, and cursor; `object` holds the serialized normalized
//! payloads keyed by `(scope, provider key)`; `fts_doc` and `event_occurrence`
//! hold the precomputed derived rows (`DerivedWrite`); `pending_op` is the outbox.
//!
//! Derived rows are deliberately **not** foreign-keyed to `object`: maintenance
//! can index a body before its object row exists, and the reference store imposes
//! no such constraint either. The object→derived tombstone cascade is therefore
//! explicit (see `scope_ops::tombstone`), not a `FOREIGN KEY … ON DELETE CASCADE`.
//!
//! The search layer is migration [`V2`]: it reshapes `fts_doc` to carry a stable
//! integer rowid and typed text columns (`subject`/`body`/`location`), builds the
//! FTS5 external-content index over it, and adds the normalized structured-filter
//! tables and junctions plus the per-chunk embedding table.
//!
//! Migration [`V3`] adds `event_occurrence.tzdata_version`: the bundled IANA
//! tzdata release each occurrence was expanded under, so a tzdata-version bump can
//! find and re-expand exactly the affected occurrences (`calendar-semantics.md`).
//!
//! `STRICT` enforces column types; the composite-key tables are `WITHOUT ROWID`
//! (clustered by their key), while `pending_op` keeps a rowid so it maps onto
//! `PendingOpId`. Time is ISO-8601 `TEXT` (sortable and exact to nanoseconds);
//! generations and ids are `INTEGER`; opaque normalized payloads are `TEXT` JSON
//! (never queried in SQL — structured filters use derived columns, not payload
//! introspection — so JSONB would only cost debuggability and portability here).

/// Migration v1: the mechanical-store base schema.
pub(crate) const V1: &str = "\
CREATE TABLE sync_scope (
    scope_key    TEXT    NOT NULL PRIMARY KEY,
    account      TEXT    NOT NULL,
    token        INTEGER NOT NULL,
    lease_expiry TEXT,
    cursor       TEXT
) STRICT;

CREATE TABLE object (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    payload      TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE fts_doc (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    fields       TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE event_occurrence (
    scope_key     TEXT NOT NULL,
    event         TEXT NOT NULL,
    start_utc     TEXT NOT NULL,
    end_utc       TEXT NOT NULL,
    recurrence_id TEXT NOT NULL,
    PRIMARY KEY (scope_key, event, start_utc, recurrence_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX event_occurrence_range
    ON event_occurrence (scope_key, start_utc, end_utc);

CREATE TABLE pending_op (
    id              INTEGER PRIMARY KEY,
    account         TEXT    NOT NULL,
    idempotency_key TEXT    NOT NULL,
    resource_key    TEXT    NOT NULL,
    depends_on      TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    token           INTEGER NOT NULL,
    lease_expiry    TEXT,
    UNIQUE (account, idempotency_key)
) STRICT;
";

/// Migration v2: the search layer.
///
/// Reshapes `fts_doc` into an FTS5 external-content source (a stable integer
/// rowid plus typed `subject`/`body`/`location` columns), builds the `fts_index`
/// virtual table over it with triggers that keep the index in sync, and adds the
/// normalized structured-filter tables (`mail_index`/`event_index` scalars and the
/// `mail_address`/`membership`/`event_participant` junctions) plus the per-chunk
/// `embedding` table. The DSL→table mapping is `north-star.md`'s Search Contract.
///
/// `fts_doc` is a re-derivable cache, so the forward-only reshape drops and
/// recreates it (a re-sync or re-index repopulates) rather than copying data — the
/// discipline `migrations.rs` documents.
pub(crate) const V2: &str = "\
DROP TABLE fts_doc;

CREATE TABLE fts_doc (
    rowid        INTEGER PRIMARY KEY,
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    subject      TEXT NOT NULL DEFAULT '',
    body         TEXT NOT NULL DEFAULT '',
    location     TEXT NOT NULL DEFAULT '',
    UNIQUE (scope_key, provider_key)
) STRICT;

CREATE VIRTUAL TABLE fts_index USING fts5 (
    subject, body, location,
    content = 'fts_doc',
    content_rowid = 'rowid',
    tokenize = 'porter unicode61'
);

CREATE TRIGGER fts_doc_ai AFTER INSERT ON fts_doc BEGIN
    INSERT INTO fts_index (rowid, subject, body, location)
    VALUES (new.rowid, new.subject, new.body, new.location);
END;

CREATE TRIGGER fts_doc_ad AFTER DELETE ON fts_doc BEGIN
    INSERT INTO fts_index (fts_index, rowid, subject, body, location)
    VALUES ('delete', old.rowid, old.subject, old.body, old.location);
END;

CREATE TRIGGER fts_doc_au AFTER UPDATE ON fts_doc BEGIN
    INSERT INTO fts_index (fts_index, rowid, subject, body, location)
    VALUES ('delete', old.rowid, old.subject, old.body, old.location);
    INSERT INTO fts_index (rowid, subject, body, location)
    VALUES (new.rowid, new.subject, new.body, new.location);
END;

CREATE TABLE mail_index (
    scope_key      TEXT    NOT NULL,
    provider_key   TEXT    NOT NULL,
    date_utc       TEXT,
    has_attachment INTEGER NOT NULL,
    thread_id      TEXT,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE INDEX mail_index_date ON mail_index (scope_key, date_utc);

CREATE TABLE mail_address (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    field        TEXT NOT NULL,
    addr         TEXT NOT NULL,
    name         TEXT,
    PRIMARY KEY (scope_key, provider_key, field, addr)
) STRICT, WITHOUT ROWID;

CREATE INDEX mail_address_lookup ON mail_address (scope_key, field, addr);

CREATE TABLE membership (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    kind         TEXT NOT NULL,
    value        TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key, kind, value)
) STRICT, WITHOUT ROWID;

CREATE INDEX membership_lookup ON membership (scope_key, kind, value);

CREATE TABLE event_index (
    scope_key      TEXT    NOT NULL,
    provider_key   TEXT    NOT NULL,
    has_conference INTEGER NOT NULL,
    my_partstat    TEXT,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE event_participant (
    scope_key    TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    role         TEXT NOT NULL,
    addr         TEXT NOT NULL,
    partstat     TEXT NOT NULL,
    PRIMARY KEY (scope_key, provider_key, role, addr)
) STRICT, WITHOUT ROWID;

CREATE INDEX event_participant_lookup ON event_participant (scope_key, role, addr);

CREATE TABLE embedding (
    scope_key    TEXT    NOT NULL,
    provider_key TEXT    NOT NULL,
    chunk_ix     INTEGER NOT NULL,
    model        TEXT    NOT NULL,
    dim          INTEGER NOT NULL,
    vector       BLOB    NOT NULL,
    PRIMARY KEY (scope_key, provider_key, chunk_ix)
) STRICT, WITHOUT ROWID;
";

/// Migration v3: per-occurrence tzdata version.
///
/// Each materialized occurrence records the bundled IANA tzdata release it was
/// expanded under (`OccurrenceRow::tzdata_version`). A tzdata-version bump
/// re-expands the affected occurrences through the maintenance path
/// (`store-and-sync.md`); the index lets that pass find occurrences expanded under
/// a stale release without a full scan. The column is **not** part of the primary
/// key — re-expansion updates it in place. The `''` default applies only to
/// hypothetical pre-V3 rows (occurrence materialization did not exist before this).
pub(crate) const V3: &str = "\
ALTER TABLE event_occurrence ADD COLUMN tzdata_version TEXT NOT NULL DEFAULT '';

CREATE INDEX event_occurrence_tzdata ON event_occurrence (tzdata_version);
";

/// Migration v4: the engine-meta key/value table.
///
/// Holds small build-level markers, currently `normalizer_version` (see
/// `engine_store::NORMALIZER_VERSION`): on open the store compares the stored value to
/// the build's and clears sync cursors when they differ, so a normalization change forces
/// a re-normalizing re-sync (`store-and-sync.md`). A pre-V4 database has no row, which
/// reads as a mismatch and triggers exactly that one-time re-sync.
pub(crate) const V4: &str = "\
CREATE TABLE meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
";

/// Migration v5: raw message-source cache metadata.
///
/// The on-demand Tier-3 raw RFC 5322 bytes (`MessageSourceCache`) live in a
/// content-addressed filesystem blob area, **not** in SQLite — a single message can
/// carry 1–15 MB of inline attachments, which would bloat the database. This table
/// holds only the per-message metadata: the SHA-256 `content_hash` naming the blob
/// file, its decoded `byte_len`, and the `fetched_at` instant (kept for future
/// quota/eviction). Keyed by `(account, provider_key)`; the bytes are deduped across
/// rows by content hash (two IMAP copies of one message share one blob).
pub(crate) const V5: &str = "\
CREATE TABLE message_source (
    account      TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    byte_len     INTEGER NOT NULL,
    fetched_at   TEXT NOT NULL,
    PRIMARY KEY (account, provider_key)
) STRICT, WITHOUT ROWID;
";
