//! Reclaiming content-addressed blobs no row names any more.
//!
//! A blob's file name is the hash of its bytes, so two copies of one message share one
//! file and no single row owns it. That is what makes the delete asymmetric: dropping a
//! `message_source` row cannot delete the file, because another row may still name the
//! same hash. So the file half is a **mark-and-sweep** — list the blob area, read the
//! hashes the store still holds, remove the difference (`store-and-sync.md`).
//!
//! Ordering carries the safety: the listing happens **before** the hash query, so a blob
//! written after it is not a candidate at all, and the candidate scan additionally skips
//! any file young enough that its own write could still be in flight.

use std::{collections::HashSet, time::SystemTime};

use engine_store::{Clock, Result, SweepReport};
use rusqlite::Connection;

use crate::{SqliteStore, blob, sql};

impl<C: Clock> SqliteStore<C> {
    /// Deletes every blob in the store's blob area whose hash no row names, returning
    /// what that reclaimed.
    ///
    /// Blobs are the bulk of a mail account on disk (raw sources run 1–15 MB apiece) and
    /// deduplication means they cannot be freed by the row delete that orphaned them, so
    /// a host runs this after anything that drops mail in quantity — narrowing sync
    /// depth, or removing an account. It takes no lease and holds no transaction: it
    /// reads a hash set and unlinks files, and a blob deleted a moment early is a cache
    /// miss the caller re-fetches, never wrong bytes — a blob read verifies that its
    /// contents still hash to the name it was found under.
    ///
    /// Run [`vacuum`](Self::vacuum) alongside it to reclaim the freed **database** pages;
    /// the two cover different halves of the same space.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a filesystem or backend failure.
    pub async fn sweep_unreferenced_blobs(&self) -> Result<SweepReport> {
        let root = self.blobs.root().to_path_buf();
        let now = SystemTime::now();
        let candidates = Self::block(move || blob::candidates(&root, now)).await?;
        if candidates.is_empty() {
            return Ok(SweepReport::default());
        }
        let live = self.read(referenced_hashes).await?;
        Self::block(move || blob::remove_unreferenced(&candidates, &live)).await
    }
}

/// Every content hash the store still names, across both blob namespaces.
fn referenced_hashes(conn: &Connection) -> Result<HashSet<String>> {
    let mut live = HashSet::new();
    for table in ["message_source", "contact_photo"] {
        let hashes: Vec<String> = sql::query_all(
            conn,
            &format!("SELECT DISTINCT content_hash FROM {table}"),
            [],
            |r| r.get(0),
        )?;
        live.extend(hashes);
    }
    Ok(live)
}

#[cfg(test)]
#[path = "sweep/tests.rs"]
mod tests;
