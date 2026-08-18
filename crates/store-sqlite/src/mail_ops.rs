//! The mail list read: the one query a mailbox list is built from.
//!
//! Every column a row renders lives in `message`, so a page costs a range scan of the ordering
//! index and nothing else — no ranking of the whole account in the caller, and no JSON payload
//! opened per row. The normalized object is still there; it is what opening a message reads.
//!
//! Three shapes, one projection:
//!
//! - **Newest** is a single ordered statement over the accounts named. Several accounts in one call
//!   is the point: "all inboxes" is a predicate, not a loop with a merge above it.
//! - **Threads** and **Keys** are targeted seeks — one cached statement per thread or key, against
//!   `message_account_thread` / `message_account_key`. A conversation is a question about a handful
//!   of messages; answering it by scanning the mailbox is what made opening one thread cost a
//!   function of how much mail the account holds.
//!
//! The order is `date_utc DESC, account DESC, scope_key DESC, provider_key DESC` throughout —
//! newest first, undated last (descending, SQLite sorts `NULL` below every value), and **total**,
//! so the window a `LIMIT` cuts does not depend on which rows the storage layer happened to visit
//! first. It is exactly the key order of `message_date` and, with the account fixed by equality, of
//! `message_account_date`, so satisfying it costs no sort.

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey, ThreadId},
    mail::{Keyword, MailFlags},
    search_index::MailRow,
    time::UtcDateTime,
    version::{ChangeKey, ETag, ModSeq, RevisionTokens},
};
use engine_store::{MailListRow, MailSelector, Result};
use rusqlite::{Connection, Row, types::Value};

use crate::{convert, sql};

/// The columns of one list row, plus the correlated memberships that say which folders it is in
/// and which keywords it carries. Both subqueries run per **emitted** row, so they cost the page,
/// not the table.
///
/// The keywords are read even though a list paints only the four system flags (already in
/// `m.flags`): this read is the message's whole mutable state, because the stored payload
/// deliberately holds none of it.
const COLUMNS: &str = "\
m.account, m.provider_key, m.thread_id, m.message_id, m.date_utc, m.flags, m.has_attachment, \
m.from_name, m.from_addr, m.subject, m.preview, \
m.last_modified, m.etag, m.change_key, m.mod_seq, \
(SELECT group_concat(b.value, char(10)) FROM membership b \
   WHERE b.scope_key = m.scope_key AND b.provider_key = m.provider_key AND b.kind = 'mailbox'), \
(SELECT group_concat(k.value, char(10)) FROM membership k \
   WHERE k.scope_key = m.scope_key AND k.provider_key = m.provider_key AND k.kind = 'keyword')";

/// Newest first, undated last, ties broken on the row's own identity — the key order of
/// `message_date`, so no sort runs.
const ORDER: &str =
    "ORDER BY m.date_utc DESC, m.account DESC, m.scope_key DESC, m.provider_key DESC";

/// The ordering index for a read over `count` accounts, named explicitly.
///
/// SQLite plans this query from an *unanalysed* schema, and left to itself it picks the index that
/// answers the account filter fastest and then sorts every matching row to satisfy the order —
/// which is the whole-mailbox cost this table exists to remove, silently, with the read still
/// returning the right rows. Naming the index makes the plan a property of the code rather than of
/// the planner's guess, and [`tests::the_windowed_read_is_ordered_by_an_index_not_a_sort`] holds it
/// there.
///
/// One account walks `message_account_date`, whose key is `(account, date_utc, …)`: the equality
/// fixes the first column and the rest is already in order. Several walk `message_date`, keyed
/// `(date_utc, account, …)`, in one descending pass with the account filter answered from the index
/// — a unified inbox is that walk, not one walk per account merged above it.
fn ordering_index(count: usize) -> &'static str {
    if count == 1 {
        "INDEXED BY message_account_date"
    } else {
        "INDEXED BY message_date"
    }
}

/// Which mail a read selects, owned so it can cross onto the blocking pool.
#[derive(Debug, Clone)]
pub(crate) enum Selector {
    /// Everything, newest first.
    Newest,
    /// Every message on any of these threads.
    Threads(Vec<ThreadId>),
    /// The messages named by these keys.
    Keys(Vec<ProviderKey>),
}

/// Takes ownership of a borrowed selector so it can cross onto the blocking pool, answering
/// `None` when it names nothing — an empty thread or key list has no rows to find, and skipping
/// the read is the difference between "no conversations to complete" and a statement that cannot
/// match.
pub(crate) fn own(select: MailSelector<'_>) -> Option<Selector> {
    match select {
        MailSelector::Newest => Some(Selector::Newest),
        MailSelector::Threads([]) | MailSelector::Keys([]) => None,
        MailSelector::Threads(threads) => Some(Selector::Threads(threads.to_vec())),
        MailSelector::Keys(keys) => Some(Selector::Keys(keys.to_vec())),
    }
}

/// Reads the rows `select` names across `accounts`, newest first, capped at `limit`.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure or a
/// corrupt stored key, instant or id.
pub(crate) fn list_mail(
    conn: &Connection,
    accounts: &[AccountId],
    select: &Selector,
    limit: usize,
) -> Result<Vec<MailListRow>> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    match select {
        Selector::Newest => newest(conn, accounts, "", limit),
        Selector::Threads(threads) => {
            let values: Vec<&str> = threads.iter().map(ThreadId::as_str).collect();
            seek(conn, accounts, "m.thread_id", &values, limit)
        }
        Selector::Keys(keys) => {
            let values: Vec<&str> = keys.iter().map(ProviderKey::as_str).collect();
            seek(conn, accounts, "m.provider_key", &values, limit)
        }
    }
}

/// The newest `limit` messages across `accounts` that hold no cached body text — the
/// body-warming work list.
///
/// Asked here rather than answered in the caller because the *warm* set is the larger half: a
/// mailbox whose bodies are all cached would otherwise have every key read out and diffed against
/// a window, on every pass, to conclude there is nothing to do.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure or a
/// corrupt stored key, instant or id.
pub(crate) fn mail_missing_body(
    conn: &Connection,
    accounts: &[AccountId],
    limit: usize,
) -> Result<Vec<MailListRow>> {
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    newest(conn, accounts, MISSING_BODY, limit)
}

/// The body cache is keyed by `(account, provider_key)`, so the absence test needs no scope.
const MISSING_BODY: &str = " AND NOT EXISTS (SELECT 1 FROM message_body cached \
     WHERE cached.account = m.account AND cached.provider_key = m.provider_key)";

/// The newest `limit` rows across `accounts` matching `extra` (an empty string for all of them):
/// one statement, ordered by the index.
///
/// The account filter is an `IN` list whose length is the number of accounts configured, so the
/// prepared statement is reused across reads rather than recompiled per call.
fn newest(
    conn: &Connection,
    accounts: &[AccountId],
    extra: &str,
    limit: usize,
) -> Result<Vec<MailListRow>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM message m {} WHERE m.account IN ({}){extra} {ORDER} LIMIT ?{}",
        ordering_index(accounts.len()),
        placeholders(accounts.len()),
        accounts.len() + 1
    );
    let mut params: Vec<Value> = accounts
        .iter()
        .map(|account| Value::Text(account.as_str().to_owned()))
        .collect();
    params.push(Value::Integer(sql_limit(limit)));
    let raw = sql::query_all(conn, &sql, rusqlite::params_from_iter(params), read_row)?;
    raw.into_iter().map(MailListRow::try_from).collect()
}

/// One targeted seek per account and per named value, merged into the same order [`newest`]
/// returns — so a conversation's members and a window's rows arrive as one sequence.
///
/// One statement per value rather than one statement with an `IN` list: the list's length changes
/// with the window, so a single statement would compile fresh SQL on every call and defeat the
/// statement cache. Each seek here is the same cached statement against an index on `column`.
fn seek(
    conn: &Connection,
    accounts: &[AccountId],
    column: &str,
    values: &[&str],
    limit: usize,
) -> Result<Vec<MailListRow>> {
    let sql = format!("SELECT {COLUMNS} FROM message m WHERE m.account = ?1 AND {column} = ?2");
    let mut raw: Vec<RawRow> = Vec::new();
    for account in accounts {
        for value in values {
            raw.extend(sql::query_all(
                conn,
                &sql,
                (account.as_str(), *value),
                read_row,
            )?);
        }
    }
    let mut rows: Vec<MailListRow> = raw
        .into_iter()
        .map(MailListRow::try_from)
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by(|a, b| order_key(b).cmp(&order_key(a)));
    rows.truncate(limit);
    Ok(rows)
}

/// The descending sort key `ORDER` applies, for the seeks that merge in memory. A seek returns
/// whole conversations rather than a window, so `scope_key` can never separate two rows that
/// `(account, provider_key)` does not already.
fn order_key(row: &MailListRow) -> (Option<UtcDateTime>, &str, &str) {
    (
        row.mail.date_utc,
        row.account.as_str(),
        row.mail.key.as_str(),
    )
}

/// One row's columns as SQLite handed them over. Collected before any validation, so the borrow of
/// the statement ends before a conversion can fail (`sql::query_all`'s contract: the mapping
/// closure reads columns and nothing else).
struct RawRow {
    account: String,
    key: String,
    thread_id: Option<String>,
    message_id: Option<String>,
    date_utc: Option<String>,
    flags: i64,
    has_attachment: i64,
    from_name: Option<String>,
    from_addr: Option<String>,
    subject: Option<String>,
    preview: Option<String>,
    last_modified: Option<String>,
    etag: Option<String>,
    change_key: Option<String>,
    mod_seq: Option<i64>,
    mailboxes: Option<String>,
    keywords: Option<String>,
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        account: row.get(0)?,
        key: row.get(1)?,
        thread_id: row.get(2)?,
        message_id: row.get(3)?,
        date_utc: row.get(4)?,
        flags: row.get(5)?,
        has_attachment: row.get(6)?,
        from_name: row.get(7)?,
        from_addr: row.get(8)?,
        subject: row.get(9)?,
        preview: row.get(10)?,
        last_modified: row.get(11)?,
        etag: row.get(12)?,
        change_key: row.get(13)?,
        mod_seq: row.get(14)?,
        mailboxes: row.get(15)?,
        keywords: row.get(16)?,
    })
}

impl TryFrom<RawRow> for MailListRow {
    type Error = engine_store::StoreError;

    fn try_from(raw: RawRow) -> Result<Self> {
        Ok(Self {
            account: AccountId::try_from(raw.account.as_str()).map_err(convert::backend)?,
            mailboxes: mailboxes(raw.mailboxes.as_deref()),
            keywords: keywords(raw.keywords.as_deref()),
            mail: MailRow {
                key: ProviderKey::new(raw.key).map_err(convert::backend)?,
                thread_id: raw
                    .thread_id
                    .as_deref()
                    .map(ThreadId::try_from)
                    .transpose()
                    .map_err(convert::backend)?,
                message_id: raw
                    .message_id
                    .map(MessageIdHeader::new)
                    .transpose()
                    .map_err(convert::backend)?,
                date_utc: convert::parse_opt_instant(raw.date_utc)?,
                flags: MailFlags::from_bits(u32::try_from(raw.flags).unwrap_or_default()),
                has_attachment: raw.has_attachment != 0,
                from_name: raw.from_name,
                from_addr: raw.from_addr,
                subject: raw.subject,
                preview: raw.preview,
                revisions: RevisionTokens {
                    etag: raw.etag.map(ETag::new),
                    schedule_tag: None,
                    change_key: raw.change_key.map(ChangeKey::new),
                    mod_seq: raw
                        .mod_seq
                        .and_then(|v| u64::try_from(v).ok())
                        .map(ModSeq::new),
                },
                last_modified: convert::parse_opt_instant(raw.last_modified)?,
            },
        })
    }
}

/// Splits the joined membership values back into collection ids.
///
/// A newline cannot appear in a collection id — IMAP mailbox names are modified UTF-7, JMAP and
/// Graph ids are opaque URL-safe text — so it separates them unambiguously. An id that no longer
/// validates is dropped rather than failing the read: it can only ever narrow what a folder view
/// shows, never widen it.
fn mailboxes(joined: Option<&str>) -> Vec<MailboxId> {
    joined
        .unwrap_or_default()
        .split('\n')
        .filter(|value| !value.is_empty())
        .filter_map(|value| MailboxId::try_from(value).ok())
        .collect()
}

/// Splits the joined keyword membership values. Same separator argument as [`mailboxes`]: a
/// keyword is an RFC 8621 atom or a provider label, and neither can contain a newline.
fn keywords(joined: Option<&str>) -> Vec<Keyword> {
    joined
        .unwrap_or_default()
        .split('\n')
        .filter(|value| !value.is_empty())
        .filter_map(|value| Keyword::new(value).ok())
        .collect()
}

/// `?1, ?2, …` for an `IN` list of `count` values.
fn placeholders(count: usize) -> String {
    (1..=count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQLite reads a negative `LIMIT` as no limit, which is what an unbounded read asks for.
fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(-1)
}

#[cfg(test)]
mod tests;

/// Whether the account holds a message that is in the graph but carries no thread.
///
/// `message_account_thread` is `(account, thread_id)`, so the null-threaded rows of one account
/// are an index range rather than a scan; the graph membership is then a primary-key probe on
/// `msgid_ref`. `LIMIT 1` because the answer is a yes/no, and in steady state there is nothing to
/// find — this runs once per mail sync and must cost nothing when it is not needed.
pub(crate) fn has_ungrouped_graphed_mail(conn: &Connection, account: &str) -> Result<bool> {
    Ok(sql::query_opt(
        conn,
        "SELECT 1 FROM message m
          WHERE m.account = ?1
            AND m.thread_id IS NULL
            AND EXISTS (SELECT 1 FROM msgid_ref r
                         WHERE r.scope_key = m.scope_key AND r.provider_key = m.provider_key)
          LIMIT 1",
        [account],
        |row| row.get::<_, i64>(0),
    )?
    .is_some())
}
