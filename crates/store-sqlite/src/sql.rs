//! Statement execution through the connection's prepared-statement cache.
//!
//! `Connection::execute` and `Connection::query_row` compile their SQL on every
//! call. On this store's hot paths that compile is most of the work: a windowed
//! list read is one point query per row shown, and a sync page is an upsert plus a
//! handful of derived-row writes per message — the same dozen statements, thousands
//! of times. Going through [`Connection::prepare_cached`] compiles each once per
//! connection and reuses it thereafter.
//!
//! The cache is per connection and bounded ([`crate::STATEMENT_CACHE`]); a statement
//! is returned to it on drop, so nothing here may hold one across a commit.

use engine_store::Result;
use rusqlite::{Connection, OptionalExtension, Params, Row};

use crate::convert::backend;

/// Runs a cached `INSERT`/`UPDATE`/`DELETE`, returning the number of rows changed.
pub(crate) fn execute<P: Params>(conn: &Connection, sql: &str, params: P) -> Result<usize> {
    conn.prepare_cached(sql)
        .map_err(backend)?
        .execute(params)
        .map_err(backend)
}

/// Runs a cached query expected to match at most one row, mapping it with `map`.
/// `None` when nothing matched.
pub(crate) fn query_opt<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: F,
) -> Result<Option<T>>
where
    P: Params,
    F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
{
    conn.prepare_cached(sql)
        .map_err(backend)?
        .query_row(params, map)
        .optional()
        .map_err(backend)
}

/// Runs a cached query and collects every row through `map`.
///
/// `map` reads columns only — it returns `rusqlite::Result`, so it cannot carry a
/// domain failure. Callers that must validate what they read (a `ProviderKey`, an
/// instant) collect the raw columns here and convert afterwards, which is also what
/// keeps the borrow of the statement short.
pub(crate) fn query_all<T, P, F>(conn: &Connection, sql: &str, params: P, map: F) -> Result<Vec<T>>
where
    P: Params,
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare_cached(sql).map_err(backend)?;
    let rows = stmt.query_map(params, map).map_err(backend)?;
    rows.collect::<rusqlite::Result<Vec<T>>>().map_err(backend)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{execute, query_all, query_opt};

    fn table() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE t (k TEXT PRIMARY KEY, v INTEGER NOT NULL) STRICT;")
            .expect("create");
        conn
    }

    #[test]
    fn a_statement_run_repeatedly_sees_its_own_earlier_writes() {
        // Reuse is invisible to a unit test — rusqlite exposes no cache counters, and
        // the win is a timing one the benches measure. What is testable is the hazard
        // reuse introduces: a cached statement returned to the cache and taken again
        // must be reset, not still positioned on the previous run's row.
        let conn = table();
        for index in 0..5i64 {
            execute(
                &conn,
                "INSERT INTO t (k, v) VALUES (?1, ?2)",
                (index.to_string(), index),
            )
            .expect("insert");
            let seen: Option<i64> = query_opt(
                &conn,
                "SELECT v FROM t WHERE k = ?1",
                [index.to_string()],
                |r| r.get(0),
            )
            .expect("query");
            assert_eq!(seen, Some(index));
        }
        assert_eq!(
            query_all(&conn, "SELECT v FROM t ORDER BY v", [], |row| row
                .get::<_, i64>(0))
            .expect("select"),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn a_cached_statement_survives_the_transaction_it_ran_in() {
        // A statement borrows the connection, so one still alive at `commit()` would
        // fail to borrow it mutably. Every helper here drops its statement before
        // returning, which is what lets the apply path run cached statements inside
        // its transaction.
        let mut conn = table();
        let tx = conn.transaction().expect("begin");
        execute(&tx, "INSERT INTO t (k, v) VALUES (?1, ?2)", ("a", 1)).expect("insert");
        tx.commit().expect("commit");
        execute(&conn, "INSERT INTO t (k, v) VALUES (?1, ?2)", ("b", 2)).expect("insert again");
        assert_eq!(
            query_all(&conn, "SELECT v FROM t ORDER BY v", [], |row| row
                .get::<_, i64>(0))
            .expect("select"),
            vec![1, 2]
        );
    }

    #[test]
    fn a_missing_row_is_none_not_an_error() {
        let conn = table();
        let found: Option<i64> =
            query_opt(&conn, "SELECT v FROM t WHERE k = ?1", ["absent"], |r| {
                r.get(0)
            })
            .expect("query");
        assert_eq!(found, None);
    }

    #[test]
    fn a_backend_failure_surfaces_as_a_store_error() {
        let conn = table();
        assert!(execute(&conn, "NOT VALID SQL", []).is_err());
    }
}
