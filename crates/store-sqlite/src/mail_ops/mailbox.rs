//! One mailbox's mail: a span of it, newest first, and how far back the store holds it.
//!
//! Both walk `message_account_date` for the one account and answer the membership per row with a
//! primary-key probe on `membership`, so neither reads a payload or sorts. The span stops at
//! `limit` rows; the oldest stops at the first row filed in the mailbox, walking up from the
//! account's oldest mail.

use engine_core::{
    ids::{AccountId, MailboxId},
    time::UtcDateTime,
};
use engine_store::{MailListRow, Result};
use rusqlite::{Connection, types::Value};

use super::{COLUMNS, ORDER, ordering_index, placeholders, read_row, sql_limit};
use crate::{convert, sql};

/// The membership probe both reads share: the row is filed in the mailbox bound to `?{n}`.
fn filed_in(parameter: usize) -> String {
    format!(
        "EXISTS (SELECT 1 FROM membership b WHERE b.scope_key = m.scope_key \
         AND b.provider_key = m.provider_key AND b.kind = 'mailbox' AND b.value = ?{parameter})"
    )
}

/// The span read over `n` accounts: the accounts, then `since`, `until`, the mailbox and the
/// limit, in that parameter order.
pub(super) fn span_sql(n: usize) -> String {
    format!(
        "SELECT {COLUMNS} FROM message m {} WHERE m.account IN ({}) \
         AND m.date_utc >= ?{} AND m.date_utc < ?{} AND {} {ORDER} LIMIT ?{}",
        ordering_index(n),
        placeholders(n),
        n + 1,
        n + 2,
        filed_in(n + 3),
        n + 4
    )
}

/// The oldest read: the account, then the mailbox.
pub(super) fn oldest_sql() -> String {
    format!(
        "SELECT m.date_utc FROM message m INDEXED BY message_account_date \
         WHERE m.account = ?1 AND m.date_utc IS NOT NULL AND {} \
         ORDER BY m.date_utc ASC LIMIT 1",
        filed_in(2)
    )
}

/// The rows across `accounts` filed in `mailbox` and dated within `[since, until)`, newest first,
/// capped at `limit`.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure or a
/// corrupt stored key, instant or id.
pub(crate) fn in_mailbox(
    conn: &Connection,
    accounts: &[AccountId],
    mailbox: &MailboxId,
    (since, until): (UtcDateTime, UtcDateTime),
    limit: usize,
) -> Result<Vec<MailListRow>> {
    let sql = span_sql(accounts.len());
    let mut params: Vec<Value> = accounts
        .iter()
        .map(|account| Value::Text(account.as_str().to_owned()))
        .collect();
    params.push(Value::Text(convert::instant_to_text(since)));
    params.push(Value::Text(convert::instant_to_text(until)));
    params.push(Value::Text(mailbox.as_str().to_owned()));
    params.push(Value::Integer(sql_limit(limit)));
    let raw = sql::query_all(conn, &sql, rusqlite::params_from_iter(params), read_row)?;
    raw.into_iter().map(MailListRow::try_from).collect()
}

/// The date of the oldest message `account` holds in `mailbox`, or `None` when it holds no
/// dated mail there.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure or a
/// corrupt stored instant.
pub(crate) fn oldest_in_mailbox(
    conn: &Connection,
    account: &AccountId,
    mailbox: &MailboxId,
) -> Result<Option<UtcDateTime>> {
    let oldest = sql::query_opt(
        conn,
        &oldest_sql(),
        (account.as_str(), mailbox.as_str()),
        |row| row.get::<_, String>(0),
    )?;
    oldest.map(|text| convert::parse_instant(&text)).transpose()
}
