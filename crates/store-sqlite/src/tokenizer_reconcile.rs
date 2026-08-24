//! Recording the FTS tokenizer a database was created with, and refusing an
//! open that requests a different one.
//!
//! The tokenizer is fixed at creation (see [`crate::options`]): this engine
//! never re-tokenizes in place — a database re-derives by re-sync — so opening
//! records the fact once into `meta` and every later open must ask for the same
//! one. The open-time counterpart of [`crate::reconcile_normalizer_version`];
//! the `configure` wiring that calls both lands with the open-options plumbing.

// The `configure` wiring is the production caller and lands in the next commit
// on this branch; until then the tests are the only callers, which the lib
// build (compiled without them) would flag as dead.
#![allow(dead_code)]

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
