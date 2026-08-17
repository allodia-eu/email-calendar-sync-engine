//! Maintaining the thread index as mail lands, instead of rescanning the account afterwards.
//!
//! A conversation is a connected component of the message-id graph (`msgid_ref`). Deriving it used
//! to mean reading every payload in the account, rebuilding the whole union-find in memory and
//! writing back what moved — after *every* sync, including the one a single mark-read triggers.
//! Here the component an incoming message joins is an indexed lookup of the ids it touches, and
//! the write is bounded by the members that actually re-keyed.
//!
//! **This is the one place the store computes rather than persists.** Everywhere else the engine
//! precomputes a `DerivedWrite` and the store writes it mechanically (`store-and-sync.md`), which
//! works because those rows are a function of the incoming object alone. A thread id is not: it is
//! a function of the incoming object *and* of what is already stored, across every scope of the
//! account. Precomputing it would mean reading the components, releasing, then writing — and a
//! concurrent scope of the same account merging into the same component in between would leave one
//! of the two writes on a stale set. The per-scope sync lease cannot fence that, because it fences
//! a scope and threading crosses scopes; the apply's transaction can, and is the only thing that
//! can.
//!
//! Two shapes are deliberately left to `engine_sync::rebuild_thread_index`: a component that a
//! *deletion* splits in two keeps one id between them, and one whose only owner was deleted keeps
//! a name no member owns. Both need the whole component walked to detect, both are rare
//! (`References` accumulates every ancestor, so removing one message almost never disconnects
//! anything), and both leave a thread that is still unique and still stable — which is what a
//! thread id is for.

use std::collections::HashMap;

use engine_core::search_index::{MailRefRow, MailRow};
use engine_store::Result;
use rusqlite::Transaction;

use crate::sql;

/// Replaces one message's `msgid_ref` rows — the same per-object replace the junctions use, so a
/// re-projection drops ids the message no longer carries and a replay is idempotent.
pub(super) fn replace_refs(
    tx: &Transaction<'_>,
    scope_key: &str,
    account: &str,
    rows: &[MailRefRow],
) -> Result<()> {
    let mut keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        sql::execute(
            tx,
            "DELETE FROM msgid_ref WHERE scope_key = ?1 AND provider_key = ?2",
            (scope_key, key),
        )?;
    }
    for row in rows {
        sql::execute(
            tx,
            "INSERT INTO msgid_ref (scope_key, provider_key, account, msgid, owned)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key, msgid)
             DO UPDATE SET owned = MAX(owned, excluded.owned)",
            rusqlite::params![
                scope_key,
                row.key.as_str(),
                account,
                row.msgid.as_str(),
                i64::from(row.owned),
            ],
        )?;
    }
    Ok(())
}

/// Assigns a thread to every message in this batch the provider did not thread, merging the
/// components its ids reach.
///
/// Runs **after** the message rows and the `msgid_ref` rows are written, so an incoming message is
/// already in the graph and a component lookup sees it.
pub(super) fn assign_threads(
    tx: &Transaction<'_>,
    scope_key: &str,
    account: &str,
    messages: &[MailRow],
    refs: &[MailRefRow],
) -> Result<()> {
    let mut by_key: HashMap<&str, Vec<&MailRefRow>> = HashMap::new();
    for row in refs {
        by_key.entry(row.key.as_str()).or_default().push(row);
    }

    // A message the provider threaded carries its thread on the object, and `upsert_message` has
    // already written it; it is not in the graph and nothing here decides it.
    let derivable: Vec<&MailRow> = messages
        .iter()
        .filter(|row| row.thread_id.is_none())
        .collect();

    let mut graphed: Vec<&MailRow> = Vec::new();
    for row in derivable {
        if by_key.contains_key(row.key.as_str()) {
            graphed.push(row);
        } else {
            // No ids at all: nothing can ever share one with it, so it is a singleton named after
            // its own key — decided without touching the graph, and stable forever. Written only
            // when the row has no thread yet: a message re-sent whole says nothing about its
            // threading, so it must not re-key itself out of whatever it is already on.
            sql::execute(
                tx,
                "UPDATE message SET thread_id = ?2
                  WHERE scope_key = ?1 AND provider_key = ?2 AND thread_id IS NULL",
                (scope_key, row.key.as_str()),
            )?;
        }
    }

    for component in components(&graphed, &by_key) {
        let ids: Vec<&str> = component
            .iter()
            .flat_map(|row| by_key[row.key.as_str()].iter().map(|r| r.msgid.as_str()))
            .collect();
        let reached = threads_touching(tx, account, &ids)?;

        // The thread is the smallest **owned** id in the merged component: the ids this batch
        // owns, plus the ids owned by every member already stored under one of the threads it
        // reaches. Falling back to the smallest provider key when the whole component owns
        // nothing — every member merely references ids nobody local has.
        let mut owned: Option<String> = component
            .iter()
            .flat_map(|row| by_key[row.key.as_str()].iter())
            .filter(|row| row.owned)
            .map(|row| row.msgid.as_str().to_owned())
            .min();
        let mut smallest_key: Option<String> = component
            .iter()
            .map(|row| row.key.as_str().to_owned())
            .min();
        if !reached.is_empty() {
            owned = smaller(owned, owned_in_threads(tx, account, &reached)?);
            smallest_key = smaller(smallest_key, key_in_threads(tx, account, &reached)?);
        }
        let Some(thread) = owned.or(smallest_key) else {
            continue;
        };

        // Re-key every member already stored under a thread this component reached. Restricted to
        // messages that are in the graph: a provider-threaded message's id is the provider's, and
        // must not be caught by a string match against a derived one.
        if !reached.is_empty() {
            rekey(tx, account, &reached, &thread)?;
        }
        for row in component {
            set_thread(tx, scope_key, row.key.as_str(), &thread)?;
        }
    }
    Ok(())
}

/// The batch's messages grouped by the ids they share **with each other**.
///
/// A cost fix, not a correctness one, and worth being honest about: the graph rows are written
/// before any assignment, so processing a page one message at a time reaches the same grouping —
/// each arrival finds the siblings already assigned and merges onto them. What it costs is a
/// *cascade*: an n-message thread arriving in one page re-keys the component built so far on
/// every step, which is quadratic in the size of the thread. Grouping first collapses that to one
/// assignment. No test can fail on this, because no observable state differs.
fn components<'a>(
    graphed: &[&'a MailRow],
    by_key: &HashMap<&str, Vec<&MailRefRow>>,
) -> Vec<Vec<&'a MailRow>> {
    let mut parent: Vec<usize> = (0..graphed.len()).collect();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, row) in graphed.iter().enumerate() {
        for id in by_key[row.key.as_str()].iter().map(|r| r.msgid.as_str()) {
            match seen.get(id) {
                Some(&other) => union(&mut parent, index, other),
                None => {
                    seen.insert(id, index);
                }
            }
        }
    }
    let mut grouped: HashMap<usize, Vec<&'a MailRow>> = HashMap::new();
    for (index, row) in graphed.iter().enumerate() {
        grouped
            .entry(find(&mut parent, index))
            .or_default()
            .push(row);
    }
    grouped.into_values().collect()
}

fn find(parent: &mut [usize], index: usize) -> usize {
    let mut root = index;
    while parent[root] != root {
        root = parent[root];
    }
    let mut node = index;
    while parent[node] != root {
        let next = parent[node];
        parent[node] = root;
        node = next;
    }
    root
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

/// The smaller of two optional ids, either of which may be absent.
fn smaller(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// The distinct threads already stored for any message touching one of `ids`.
fn threads_touching(tx: &Transaction<'_>, account: &str, ids: &[&str]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sql::query_all(
        tx,
        &format!(
            "SELECT DISTINCT m.thread_id
               FROM msgid_ref r
               JOIN message m ON m.scope_key = r.scope_key AND m.provider_key = r.provider_key
              WHERE r.account = ?1 AND m.thread_id IS NOT NULL AND r.msgid IN ({})",
            placeholders(ids.len())
        ),
        rusqlite::params_from_iter(std::iter::once(account).chain(ids.iter().copied())),
        |row| row.get::<_, String>(0),
    )
}

/// The smallest **owned** id among the members of `threads`.
fn owned_in_threads(
    tx: &Transaction<'_>,
    account: &str,
    threads: &[String],
) -> Result<Option<String>> {
    sql::query_opt(
        tx,
        &format!(
            "SELECT MIN(r.msgid)
               FROM msgid_ref r
               JOIN message m ON m.scope_key = r.scope_key AND m.provider_key = r.provider_key
              WHERE m.account = ?1 AND r.owned = 1 AND m.thread_id IN ({})",
            placeholders(threads.len())
        ),
        rusqlite::params_from_iter(
            std::iter::once(account).chain(threads.iter().map(String::as_str)),
        ),
        |row| row.get::<_, Option<String>>(0),
    )
    .map(Option::flatten)
}

/// The smallest provider key among the members of `threads` — the fallback name for a component
/// that owns no id at all.
fn key_in_threads(
    tx: &Transaction<'_>,
    account: &str,
    threads: &[String],
) -> Result<Option<String>> {
    sql::query_opt(
        tx,
        &format!(
            "SELECT MIN(provider_key) FROM message
              WHERE account = ?1 AND thread_id IN ({})",
            placeholders(threads.len())
        ),
        rusqlite::params_from_iter(
            std::iter::once(account).chain(threads.iter().map(String::as_str)),
        ),
        |row| row.get::<_, Option<String>>(0),
    )
    .map(Option::flatten)
}

/// Moves every graphed member of `threads` onto `thread`.
fn rekey(tx: &Transaction<'_>, account: &str, threads: &[String], thread: &str) -> Result<()> {
    sql::execute(
        tx,
        &format!(
            "UPDATE message SET thread_id = ?1
              WHERE account = ?2
                AND thread_id IN ({})
                AND EXISTS (SELECT 1 FROM msgid_ref r
                             WHERE r.scope_key = message.scope_key
                               AND r.provider_key = message.provider_key)",
            placeholders(threads.len())
        ),
        rusqlite::params_from_iter(
            [thread, account]
                .into_iter()
                .chain(threads.iter().map(String::as_str)),
        ),
    )?;
    Ok(())
}

fn set_thread(tx: &Transaction<'_>, scope_key: &str, key: &str, thread: &str) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE message SET thread_id = ?3 WHERE scope_key = ?1 AND provider_key = ?2",
        (scope_key, key, thread),
    )?;
    Ok(())
}

/// `?,?,?` for an `IN` list of `n` bound values.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}
