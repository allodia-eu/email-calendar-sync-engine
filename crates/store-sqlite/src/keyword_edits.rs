//! Showing a queued keyword change before the server confirms it ([`KeywordEdit`]).
//!
//! Three moments write a message's keywords on an op's behalf, each inside the transaction that
//! causes it:
//!
//! - **Queued** ([`queued`]): the change is shown at once. The keywords the message held become its
//!   server set, unless an earlier change already recorded one.
//! - **A sync wrote the message** ([`synced`]): what the sync wrote is the server's word, so it
//!   becomes the server set, and every change still unsettled is applied on top again. A change the
//!   server accepted after the pass began is applied as well: the pass may have read the message
//!   before the change landed.
//! - **Settled** ([`settled`]): an accepted change joins the server set; one refused for good, or
//!   withdrawn, is simply no longer applied. Either way the message shows the server set with the
//!   changes still unsettled, and the server set is dropped once none are.
//!
//! A message the store does not hold is left alone at every step: the pass that admits it brings
//! its keywords, and those already include whatever the server accepted.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, MailFlags},
    search_index::MembershipKind,
    time::UtcDateTime,
    write::KeywordEdit,
};
use engine_store::{PendingOpState, Result};
use rusqlite::{OptionalExtension, Transaction};

use crate::{convert, derived_ops::replace_kind, sql};

/// Shows `edit`, just queued by one of `account`'s ops.
pub(crate) fn queued(tx: &Transaction<'_>, account: &str, edit: &KeywordEdit) -> Result<()> {
    let key = edit.key.as_str();
    let Some(held) = held_keywords(tx, account, key)? else {
        return Ok(());
    };
    let server = if let Some(server) = server_set(tx, account, key)? {
        server
    } else {
        store_server_set(tx, account, key, &held)?;
        held
    };
    let unsettled = unsettled_edits(tx, account, key)?;
    show(tx, account, key, &KeywordEdit::shown(&server, &unsettled))
}

/// Re-applies the queued changes to the messages a sync just wrote in `scope_key`.
///
/// `observed_from` is when the pass began reading the server; a change accepted after it may be
/// missing from what the pass read. `None` reads as "now", so no accepted change is re-applied.
pub(crate) fn synced(
    tx: &Transaction<'_>,
    scope_key: &str,
    keys: impl IntoIterator<Item = impl AsRef<str>>,
    observed_from: Option<UtcDateTime>,
) -> Result<()> {
    if !any_to_show(tx, observed_from)? {
        return Ok(());
    }
    for key in keys {
        let key = key.as_ref();
        let Some(account) = scope_row_account(tx, scope_key, key)? else {
            continue;
        };
        let server = row_keywords(tx, scope_key, key)?;
        let accepted = observed_from.map_or(Ok(Vec::new()), |since| {
            accepted_since(tx, &account, key, since)
        })?;
        let unsettled = unsettled_edits(tx, &account, key)?;
        if unsettled.is_empty() && accepted.is_empty() {
            continue;
        }
        let server = KeywordEdit::shown(&server, &accepted);
        if unsettled.is_empty() {
            show(tx, &account, key, &server)?;
        } else {
            store_server_set(tx, &account, key, &server)?;
            show(tx, &account, key, &KeywordEdit::shown(&server, &unsettled))?;
        }
    }
    Ok(())
}

/// Settles `edit`'s place in what `account`'s message shows, its op having just reached `state`.
pub(crate) fn settled(
    tx: &Transaction<'_>,
    account: &str,
    edit: &KeywordEdit,
    state: PendingOpState,
) -> Result<()> {
    let key = edit.key.as_str();
    let Some(mut server) = server_set(tx, account, key)? else {
        return Ok(());
    };
    if state == PendingOpState::Succeeded {
        edit.apply_to(&mut server);
    }
    let unsettled = unsettled_edits(tx, account, key)?;
    if unsettled.is_empty() {
        sql::execute(
            tx,
            "DELETE FROM server_keywords WHERE account = ?1 AND provider_key = ?2",
            (account, key),
        )?;
    } else {
        store_server_set(tx, account, key, &server)?;
    }
    show(tx, account, key, &KeywordEdit::shown(&server, &unsettled))
}

/// The unsettled changes to `key`, in the order they were queued.
fn unsettled_edits(tx: &Transaction<'_>, account: &str, key: &str) -> Result<Vec<KeywordEdit>> {
    edits_where(
        tx,
        "SELECT keyword_edit FROM pending_op
          WHERE account = ?1 AND keyword_edit IS NOT NULL
            AND state IN ('Pending', 'InFlight', 'NeedsConfirmation')
          ORDER BY id",
        (account,),
        key,
    )
}

/// The changes to `key` the server accepted after `since`, in the order they were queued.
fn accepted_since(
    tx: &Transaction<'_>,
    account: &str,
    key: &str,
    since: UtcDateTime,
) -> Result<Vec<KeywordEdit>> {
    Ok(accepted_after(tx, Some(account), since)?
        .into_iter()
        .filter(|edit| edit.key.as_str() == key)
        .collect())
}

/// The changes accepted after `since`, `account`'s alone when given, in the order queued.
///
/// The text of an instant drops the trailing zeros of its fraction, so two instants within one
/// second do not sort as text. SQL narrows by the whole-second prefix, which is fixed-width and
/// does sort, and the exact comparison is made on the parsed instant.
fn accepted_after(
    tx: &Transaction<'_>,
    account: Option<&str>,
    since: UtcDateTime,
) -> Result<Vec<KeywordEdit>> {
    let mut stmt = tx
        .prepare(
            "SELECT keyword_edit, settled_at FROM pending_op
              WHERE (?1 IS NULL OR account = ?1) AND keyword_edit IS NOT NULL
                AND state = 'Succeeded' AND substr(settled_at, 1, 19) >= substr(?2, 1, 19)
              ORDER BY id",
        )
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map((account, convert::instant_to_text(since)), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(convert::backend)?;
    let mut edits = Vec::new();
    for row in rows {
        let (edit, settled_at) = row.map_err(convert::backend)?;
        if convert::parse_instant(&settled_at)? > since {
            edits.push(serde_json::from_str(&edit).map_err(convert::backend)?);
        }
    }
    Ok(edits)
}

fn edits_where(
    tx: &Transaction<'_>,
    query: &str,
    params: impl rusqlite::Params,
    key: &str,
) -> Result<Vec<KeywordEdit>> {
    let mut stmt = tx.prepare(query).map_err(convert::backend)?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(convert::backend)?;
    let mut edits = Vec::new();
    for row in rows {
        let edit: KeywordEdit =
            serde_json::from_str(&row.map_err(convert::backend)?).map_err(convert::backend)?;
        if edit.key.as_str() == key {
            edits.push(edit);
        }
    }
    Ok(edits)
}

/// Whether any op could change what a just-synced message shows: one carrying a change that is
/// unsettled, or accepted after `since`. Spares a pass with nothing queued a lookup per message.
fn any_to_show(tx: &Transaction<'_>, since: Option<UtcDateTime>) -> Result<bool> {
    let unsettled: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM pending_op
              WHERE keyword_edit IS NOT NULL
                AND state IN ('Pending', 'InFlight', 'NeedsConfirmation')
              LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    if unsettled.is_some() {
        return Ok(true);
    }
    let Some(since) = since else {
        return Ok(false);
    };
    Ok(!accepted_after(tx, None, since)?.is_empty())
}

fn server_set(tx: &Transaction<'_>, account: &str, key: &str) -> Result<Option<BTreeSet<Keyword>>> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT keywords FROM server_keywords WHERE account = ?1 AND provider_key = ?2",
            (account, key),
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    stored
        .map(|json| serde_json::from_str(&json).map_err(convert::backend))
        .transpose()
}

fn store_server_set(
    tx: &Transaction<'_>,
    account: &str,
    key: &str,
    keywords: &BTreeSet<Keyword>,
) -> Result<()> {
    let json = serde_json::to_string(keywords).map_err(convert::backend)?;
    sql::execute(
        tx,
        "INSERT INTO server_keywords (account, provider_key, keywords) VALUES (?1, ?2, ?3)
         ON CONFLICT(account, provider_key) DO UPDATE SET keywords = excluded.keywords",
        (account, key, json),
    )?;
    Ok(())
}

/// The keywords one of `account`'s rows for `key` holds, or `None` when the store holds none.
fn held_keywords(
    tx: &Transaction<'_>,
    account: &str,
    key: &str,
) -> Result<Option<BTreeSet<Keyword>>> {
    let scope: Option<String> = tx
        .query_row(
            "SELECT scope_key FROM message WHERE account = ?1 AND provider_key = ?2 LIMIT 1",
            (account, key),
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    scope.map(|scope| row_keywords(tx, &scope, key)).transpose()
}

fn scope_row_account(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT account FROM message WHERE scope_key = ?1 AND provider_key = ?2",
        (scope_key, key),
        |row| row.get(0),
    )
    .optional()
    .map_err(convert::backend)
}

fn row_keywords(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<BTreeSet<Keyword>> {
    let mut stmt = tx
        .prepare(
            "SELECT value FROM membership
              WHERE scope_key = ?1 AND provider_key = ?2 AND kind = ?3",
        )
        .map_err(convert::backend)?;
    let values = stmt
        .query_map(
            (
                scope_key,
                key,
                convert::membership_kind_text(MembershipKind::Keyword),
            ),
            |row| row.get::<_, String>(0),
        )
        .map_err(convert::backend)?;
    let mut keywords = BTreeSet::new();
    for value in values {
        // A value no keyword parses as was not written by this store; it is not this code's to
        // drop, and it cannot be shown either.
        if let Ok(keyword) = Keyword::try_from(value.map_err(convert::backend)?) {
            keywords.insert(keyword);
        }
    }
    Ok(keywords)
}

/// Writes `keywords` as what every one of `account`'s rows for `key` shows: the `flags` bitfield
/// and the keyword memberships, the two places a message's keywords live.
fn show(
    tx: &Transaction<'_>,
    account: &str,
    key: &str,
    keywords: &BTreeSet<Keyword>,
) -> Result<()> {
    let flags = i64::from(MailFlags::from_keywords(keywords).bits());
    let values: Vec<String> = keywords.iter().map(|k| k.as_str().to_owned()).collect();
    let scopes: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT scope_key FROM message WHERE account = ?1 AND provider_key = ?2")
            .map_err(convert::backend)?;
        let rows = stmt
            .query_map((account, key), |row| row.get::<_, String>(0))
            .map_err(convert::backend)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(convert::backend)?
    };
    for scope in scopes {
        sql::execute(
            tx,
            "UPDATE message SET flags = ?3 WHERE scope_key = ?1 AND provider_key = ?2",
            (scope.as_str(), key, flags),
        )?;
        replace_kind(tx, &scope, key, MembershipKind::Keyword, &values)?;
    }
    Ok(())
}
