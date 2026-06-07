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
//! The searchable FTS5 virtual table and the structured filter index are layered
//! over `fts_doc`/`object` in the search sub-step as later migration versions;
//! `fts_doc` is the FTS5 external-content source, so storing the field text now
//! needs no schema change later.
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
