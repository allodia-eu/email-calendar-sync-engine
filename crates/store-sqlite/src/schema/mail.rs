//! The mail row and the message-id graph: schema steps v8 through v10.
//!
//! Split from [`super`], which holds the base store, the search layer and the calendar steps.
//! These three are one story — a message's mutable half becoming a table, and the graph its
//! conversation is a component of — and they are the steps a mail change touches.

/// Migration v8: the thread lookup `mail_index` never had.
///
/// The table carried a date index and nothing on `thread_id`, so gathering a
/// conversation's members meant reading every row of every folder and comparing in
/// the caller — a whole-mailbox scan to answer a question about a handful of
/// messages. `mail_index` is `WITHOUT ROWID` keyed on `(scope_key, provider_key)`, so
/// this index carries the provider key alongside and answers
/// `derived_ops::scope_thread_keys` without touching the table at all.
///
/// It is created on an existing database, so the first open after an upgrade builds
/// it over the mail already stored.
pub(crate) const V8: &str = "\
CREATE INDEX mail_index_thread ON mail_index (scope_key, thread_id);
";

/// Migration v9: `message` — a mail row, and the retirement of `mail_index`.
///
/// `mail_index` carried the keys a *filter* needs and nothing a row *shows*, so a list read
/// ranked every message in the account by date and then opened the surviving payloads one JSON
/// document at a time. This table carries what the row shows, so the first page costs the size of
/// the page. `object` stays the canonical normalized record and leaves the list path entirely;
/// opening a message still reads it.
///
/// **This table is a message's mutable half.** Its immutable half — headers, MIME tree, body —
/// never changes once the server holds it and lives in `object` as `MailContent`. Everything
/// here moves without those bytes moving: the keyword bitfield, the derived thread, the revision
/// tokens and `last_modified` that bump whenever any of it does. One home per fact, so there is
/// no second copy to disagree (`store-and-sync.md`).
///
/// `account` is denormalized onto the row — derivable from `scope_key` through `sync_scope`, and
/// kept here anyway — because "all inboxes" is then a predicate over one ordered index instead of
/// a query per account merged in the caller. It is filled from `sync_scope` on every write, so it
/// cannot disagree with the scope.
///
/// The four indices each answer one read and no more: `message_date` the unified newest-first
/// window (with `account` in the key so the filter is answered from the index rather than by
/// reading each candidate row), `message_account_date` one account's, `message_account_thread` a
/// conversation's members, `message_account_key` a named message. The primary key stays
/// `(scope_key, provider_key)`, matching every other derived table, so the tombstone cascade in
/// `derived_ops::delete_derived_rows` reaches this table the same way as the rest.
///
/// `flags` is the system-keyword bitfield ([`engine_core::mail::MailFlags`]): the only keywords a
/// row's appearance depends on, where a sort or a filter must not pay a junction join for them.
/// Every keyword, system and user alike, still lands in `membership` — that is where a set of
/// arbitrary cardinality belongs, and it is what `keyword:` searches.
///
/// `RevisionTokens` also carries a `schedule_tag`; that is CalDAV scheduling state, which a
/// message can never have, so it gets no column.
///
/// No backfill: `mail_index` is dropped in the same step, and no store carrying rows in it
/// survives to reach this version.
pub(crate) const V9: &str = "\
CREATE TABLE message (
    scope_key      TEXT    NOT NULL,
    provider_key   TEXT    NOT NULL,
    account        TEXT    NOT NULL,
    thread_id      TEXT,
    message_id     TEXT,
    date_utc       TEXT,
    flags          INTEGER NOT NULL,
    has_attachment INTEGER NOT NULL,
    from_name      TEXT,
    from_addr      TEXT,
    subject        TEXT,
    preview        TEXT,
    last_modified  TEXT,
    etag           TEXT,
    change_key     TEXT,
    mod_seq        INTEGER,
    PRIMARY KEY (scope_key, provider_key)
) STRICT, WITHOUT ROWID;

CREATE INDEX message_date           ON message (date_utc, account);
CREATE INDEX message_account_date   ON message (account, date_utc);
CREATE INDEX message_account_thread ON message (account, thread_id);
CREATE INDEX message_account_key    ON message (account, provider_key);

DROP TABLE mail_index;
";

/// Migration v10: `msgid_ref` — the message-id graph, so threading stops rescanning the account.
///
/// A conversation is a connected component of the ids messages own (`Message-ID`) and reference
/// (`In-Reply-To`, `References`). That graph only ever existed in memory: every sync read *every*
/// payload in the account, rebuilt the whole union-find, and wrote back what had moved — after a
/// pass that may have changed one flag. Stored as rows, the component an incoming message joins is
/// an indexed lookup of the ids it touches, and the write is bounded by the members that actually
/// re-keyed.
///
/// `owned` separates the two kinds of id because they answer different questions: any id joins a
/// message to a component, but only an owned one can *name* the resulting thread. Two replies to a
/// root nobody has yet still belong together, and the thread is named after one of them.
///
/// Only **derivable** messages are here. A provider that assigns thread ids is authoritative, so
/// its messages project no rows and the table *is* the derivable set — a forged `References`
/// header has nothing to reach rather than a filter every reader must remember. A derivable
/// message with no headers at all projects no rows either: nothing can ever share an id with it,
/// so it is a singleton named after its own provider key and needs no entry to stay one.
///
/// `account` is denormalized onto the row for the same reason it is on `message`: the lookup that
/// matters is "which components in this account touch these ids", across every folder, and
/// threading is cross-scope by definition.
///
/// **Backfilled**, because the alternative is a re-sync: an existing store holds the payloads this
/// graph is a projection of, so [`crate::backfill`] rebuilds it from `object` in the same
/// transaction as the DDL. A database is therefore never at v10 with an empty graph, which would
/// read as "no message shares an id with any other" and strand every later reply in a thread of
/// its own.
pub(crate) const V10: &str = "\
CREATE TABLE msgid_ref (
    scope_key    TEXT    NOT NULL,
    provider_key TEXT    NOT NULL,
    account      TEXT    NOT NULL,
    msgid        TEXT    NOT NULL,
    owned        INTEGER NOT NULL,
    PRIMARY KEY (scope_key, provider_key, msgid)
) STRICT, WITHOUT ROWID;

CREATE INDEX msgid_ref_lookup ON msgid_ref (account, msgid);
";
