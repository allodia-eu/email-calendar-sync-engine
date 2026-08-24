//! Refusing an open that would mix FTS tokenizers, and recording the one a
//! database was created with.
//!
//! The tokenizer is fixed at creation (see [`crate::options`]): this engine
//! never re-tokenizes in place — a database re-derives by re-sync — so every
//! open must ask for the one the database's index already carries. The ground
//! truth is the index's own DDL: any database with an `fts_index` table has a
//! tokenizer, whatever its schema version or `meta` state (`fts_index` ships in
//! migration V2, the `meta` table only in V4, so meta presence can never stand
//! in for index presence). [`classify`] derives the tokenizer from the DDL's
//! `tokenize = '…'` clause, consulting `meta.fts_tokenizer` as a recorded cache
//! of that answer when the row exists.
//!
//! [`ensure_compatible`] runs **before** [`crate::migrations::migrate`]: a gate
//! whose semantics are "refuse and leave the database as it was" must not let
//! migrate first build the missing FTS shapes under the requested tokenizer.
//! [`record`] then fills the meta row after migrate — which also closes the
//! crash window where a database whose record insert never landed (process
//! death between the migrate commit and the insert) would otherwise read back
//! as the default; the DDL still tells the truth, so the next open classifies
//! correctly and repairs the cache.

use engine_store::Result;
use rusqlite::Connection;

use crate::{convert::backend, options::FtsTokenizer};

/// What open-time inspection found before `migrate` ran. `Fresh` ⇔ the database
/// has no FTS index yet (a true v0 one, or a v1 one — either way this open
/// shapes it); `Known(t)` ⇔ the tokenizer the database's `fts_index` was built
/// with — the recorded meta row when present, otherwise the index's own DDL.
#[derive(Clone, Copy)]
pub(crate) enum FtsTokenizerKnown {
    Fresh,
    Known(FtsTokenizer),
}

/// Refuses an open whose requested tokenizer differs from the one the
/// database's index already carries. Pure — no database work — so it is
/// decidable, and refused, **before** [`crate::migrations::migrate`] runs and
/// mutates anything.
pub(crate) fn ensure_compatible(found: FtsTokenizerKnown, requested: FtsTokenizer) -> Result<()> {
    let FtsTokenizerKnown::Known(stored) = found else {
        return Ok(());
    };
    if stored != requested {
        return Err(engine_store::StoreError::Backend(format!(
            "fts tokenizer mismatch: database was created with '{}' but open \
             requested '{}'; this engine does not re-tokenize in place — \
             recreate the database (its contents re-derive by re-sync)",
            stored.sql(),
            requested.sql()
        )));
    }
    Ok(())
}

/// Records `requested` into `meta.fts_tokenizer`, filling the cache when the
/// row is absent: a database this open created, or one whose earlier record
/// insert never landed. A present row is left alone — by the time this runs,
/// [`ensure_compatible`] has already refused any request that disagrees with
/// it. Needs the `meta` table, so it runs after `migrate`.
pub(crate) fn record(conn: &Connection, requested: FtsTokenizer) -> Result<()> {
    use rusqlite::OptionalExtension;
    let recorded = conn
        .query_row("SELECT 1 FROM meta WHERE key = 'fts_tokenizer'", [], |_| {
            Ok(())
        })
        .optional()
        .map_err(backend)?;
    if recorded.is_none() {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('fts_tokenizer', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [requested.sql()],
        )
        .map_err(backend)?;
    }
    Ok(())
}

/// Classifies what the database already knows about its FTS tokenizer, so the
/// open can distinguish a database it is free to shape from one it must accept
/// as-is. Reads the catalog once, before `migrate`: a `meta.fts_tokenizer` row
/// wins (it is the recorded cache); otherwise an `fts_index` table yields its
/// tokenizer from the DDL — the ground truth for any database that already has
/// an index, whatever its version or meta state; no index at all ⇒
/// [`FtsTokenizerKnown::Fresh`]. Existence comes from `sqlite_master` rather
/// than a bare `SELECT … FROM meta`, which would error on every database below
/// v4 (the meta table arrives with V4, the index with V2).
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure, or for
/// a recorded or derived value this build does not recognize (corruption).
pub(crate) fn classify(conn: &Connection) -> Result<FtsTokenizerKnown> {
    use rusqlite::OptionalExtension;
    let mut index_ddl: Option<String> = None;
    let mut meta_exists = false;
    let mut probe = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE name IN ('fts_index', 'meta')")
        .map_err(backend)?;
    let mut rows = probe.query([]).map_err(backend)?;
    while let Some(row) = rows.next().map_err(backend)? {
        let name: String = row.get(0).map_err(backend)?;
        if name == "meta" {
            meta_exists = true;
        } else {
            index_ddl = row.get(1).map_err(backend)?;
        }
    }
    if meta_exists {
        let recorded = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(value) = recorded {
            return Ok(FtsTokenizerKnown::Known(
                FtsTokenizer::from_meta(&value)
                    .ok_or_else(|| backend(format!("unknown fts_tokenizer meta value: {value}")))?,
            ));
        }
    }
    match index_ddl {
        Some(ddl) => Ok(FtsTokenizerKnown::Known(tokenizer_from_ddl(&ddl)?)),
        None => Ok(FtsTokenizerKnown::Fresh),
    }
}

/// Derives the tokenizer from an FTS5 `CREATE VIRTUAL TABLE` DDL by reading its
/// `tokenize = '…'` clause — the clause text is exactly an
/// [`FtsTokenizer::sql`] string, because the migrations write it from there.
fn tokenizer_from_ddl(ddl: &str) -> Result<FtsTokenizer> {
    const CLAUSE: &str = "tokenize = '";
    let start = ddl
        .find(CLAUSE)
        .map(|at| at + CLAUSE.len())
        .ok_or_else(|| backend("fts_index DDL carries no tokenize clause"))?;
    let end = ddl[start..]
        .find('\'')
        .map(|offset| start + offset)
        .ok_or_else(|| backend("fts_index tokenize clause is unterminated"))?;
    FtsTokenizer::from_meta(&ddl[start..end]).ok_or_else(|| {
        backend(format!(
            "unknown fts tokenizer clause: {}",
            &ddl[start..end]
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::FtsTokenizer;

    #[test]
    fn a_connection_without_an_fts_index_classifies_as_fresh() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            matches!(classify(&conn), Ok(FtsTokenizerKnown::Fresh)),
            "no fts_index at all means the open is creating the shape"
        );
    }

    /// The shape every schema-v2/v3 database has: `fts_index` from V2 (porter)
    /// and no `meta` table until V4. The tokenizer comes from the DDL — the
    /// index itself — never from meta-table presence.
    #[test]
    fn a_v2_shaped_database_classifies_from_the_index_ddl() {
        for tokenizer in [FtsTokenizer::PorterUnicode61, FtsTokenizer::Trigram] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(crate::schema::V1).unwrap();
            conn.execute_batch(&crate::schema::v2(tokenizer)).unwrap();
            assert!(
                matches!(
                    classify(&conn),
                    Ok(FtsTokenizerKnown::Known(found)) if found == tokenizer
                ),
                "{tokenizer:?}: an existing fts_index is a tokenizer, whatever the meta state"
            );
        }
    }

    #[test]
    fn a_recorded_row_classifies_as_known_and_an_unknown_value_errors() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
        // Before the row exists, the DDL already classifies the database.
        assert!(matches!(
            classify(&conn),
            Ok(FtsTokenizerKnown::Known(FtsTokenizer::Trigram))
        ));
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('fts_tokenizer', 'trigram')",
            [],
        )
        .unwrap();
        assert!(matches!(
            classify(&conn),
            Ok(FtsTokenizerKnown::Known(FtsTokenizer::Trigram))
        ));
        conn.execute(
            "UPDATE meta SET value = 'soundex' WHERE key = 'fts_tokenizer'",
            [],
        )
        .unwrap();
        assert!(
            classify(&conn).is_err(),
            "a value this build does not recognize is corruption, not a guess"
        );
    }

    #[test]
    fn the_ddl_clause_reader_derives_or_reports_corruption() {
        for (tokenizer, clause) in [
            (FtsTokenizer::PorterUnicode61, "porter unicode61"),
            (FtsTokenizer::Trigram, "trigram"),
        ] {
            let ddl = format!(
                "CREATE VIRTUAL TABLE fts_index USING fts5 (subject, body, location, content = 'fts_doc', content_rowid = 'rowid', tokenize = '{clause}')"
            );
            assert!(matches!(
                tokenizer_from_ddl(&ddl),
                Ok(found) if found == tokenizer
            ));
        }
        assert!(
            tokenizer_from_ddl("CREATE TABLE t (a)").is_err(),
            "no tokenize clause"
        );
        assert!(
            tokenizer_from_ddl("tokenize = 'trigram").is_err(),
            "an unterminated clause"
        );
        assert!(
            tokenizer_from_ddl("tokenize = 'soundex'").is_err(),
            "a tokenizer this build does not know"
        );
    }
}
