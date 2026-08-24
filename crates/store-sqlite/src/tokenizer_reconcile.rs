//! Recording the FTS tokenizer a database was created with, and refusing an
//! open that requests a different one.
//!
//! The tokenizer is fixed at creation (see [`crate::options`]): this engine
//! never re-tokenizes in place — a database re-derives by re-sync — so opening
//! records the fact once into `meta` and every later open must ask for the same
//! one. [`classify`] inspects the database **before** `migrate` runs (migrate
//! creates the `meta` table, which would erase the fresh/pre-option
//! distinction), and [`reconcile_fts_tokenizer`] is the open-time counterpart
//! of [`crate::reconcile_normalizer_version`].

use engine_store::Result;
use rusqlite::Connection;

use crate::{convert::backend, options::FtsTokenizer};

/// What open-time inspection found before `migrate` ran. `Fresh` ⇔ the meta
/// table itself was absent (a database this open creates); `PreOption` ⇔ the
/// table existed without an `fts_tokenizer` row (created before the option);
/// `Known` ⇔ the row this database was created under.
#[derive(Clone, Copy)]
pub(crate) enum FtsTokenizerKnown {
    Fresh,
    PreOption,
    Known(FtsTokenizer),
}

pub(crate) fn reconcile_fts_tokenizer(
    found: FtsTokenizerKnown,
    conn: &Connection,
    requested: FtsTokenizer,
) -> Result<()> {
    let stored = match found {
        FtsTokenizerKnown::Fresh => requested,
        FtsTokenizerKnown::PreOption => FtsTokenizer::PorterUnicode61,
        FtsTokenizerKnown::Known(t) => t,
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
    if matches!(
        found,
        FtsTokenizerKnown::Fresh | FtsTokenizerKnown::PreOption
    ) {
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
/// as-is. Must run **before** [`crate::migrations::migrate`], which creates the
/// `meta` table and would erase the fresh/pre-option distinction: no `meta`
/// table ⇒ [`FtsTokenizerKnown::Fresh`]; the table without an `fts_tokenizer`
/// row ⇒ [`FtsTokenizerKnown::PreOption`]; the row ⇒ [`FtsTokenizerKnown::Known`].
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure, or for a
/// recorded value this build does not recognize (corruption).
pub(crate) fn classify(conn: &Connection) -> Result<FtsTokenizerKnown> {
    use rusqlite::OptionalExtension;
    let found = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional();
    match found {
        Ok(Some(value)) => Ok(FtsTokenizerKnown::Known(
            FtsTokenizer::from_meta(&value)
                .ok_or_else(|| backend(format!("unknown fts_tokenizer meta value: {value}")))?,
        )),
        Ok(None) => Ok(FtsTokenizerKnown::PreOption),
        Err(err) if is_no_such_table(&err) => Ok(FtsTokenizerKnown::Fresh),
        Err(err) => Err(backend(err)),
    }
}

/// Whether `err` is SQLite's "the queried table does not exist" — the answer a
/// database with no `meta` table yet (a fresh one) gives the probe in
/// [`classify`]. Determined empirically against the bundled SQLite, which
/// surfaces it as `SqliteFailure` with primary code `Unknown` and the message
/// `"no such table: meta"` — the message, not the code, is the discriminator.
fn is_no_such_table(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("no such table")
    )
}

#[cfg(test)]
mod tests {
    use rusqlite::OptionalExtension as _;

    use super::*;
    use crate::options::FtsTokenizer;

    #[test]
    fn a_connection_without_the_meta_table_classifies_as_fresh() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            matches!(classify(&conn), Ok(FtsTokenizerKnown::Fresh)),
            "no meta table at all means the open is creating the database"
        );
    }

    #[test]
    fn a_meta_table_without_the_row_classifies_as_pre_option() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();
        assert!(
            matches!(classify(&conn), Ok(FtsTokenizerKnown::PreOption)),
            "the table without the row means the database predates the option"
        );
    }

    #[test]
    fn a_recorded_row_classifies_as_known_and_an_unknown_value_errors() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
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
    fn is_no_such_table_matches_only_a_missing_table() {
        let conn = Connection::open_in_memory().unwrap();
        let missing_table = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_err();
        assert!(is_no_such_table(&missing_table), "{missing_table:?}");
        // The same SqliteFailure shape with a different message must not match.
        conn.execute_batch("CREATE TABLE t (a)").unwrap();
        let missing_column = conn
            .query_row("SELECT b FROM t", [], |row| row.get::<_, String>(0))
            .unwrap_err();
        assert!(!is_no_such_table(&missing_column), "{missing_column:?}");
    }
}
