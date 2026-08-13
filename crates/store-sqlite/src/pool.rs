//! One writer connection, and the readers that no longer queue behind it.
//!
//! WAL admits one writer *and* many readers at once. Routing every read through the
//! writer's mutex spends that guarantee: a streaming sync's commits block the list
//! read a user is waiting on. A file database therefore opens [`READERS`] further
//! connections onto the same file, and reads take whichever is free.
//!
//! Each reader is `query_only`, so a write routed to one fails loudly instead of
//! quietly serializing behind the writer again — the routing is a choice per call
//! site, and a choice nothing checks is one that drifts.
//!
//! An in-memory database cannot be split: one connection *is* the database, and a
//! second `:memory:` open would be a different, empty one. Those stores hold a
//! single connection and read through the writer.

use std::{
    path::Path,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
};

use engine_store::Result;
use rusqlite::Connection;

use crate::convert::backend;

/// The mmap window for file-backed databases (256 MiB): fewer read syscalls on the
/// hot search path, so query cost tracks index size.
const MMAP_BYTES: i64 = 256 * 1024 * 1024;

/// The page cache each connection keeps, in KiB (SQLite reads a negative `cache_size`
/// that way; a positive one counts *pages*, which changes meaning with the page size).
///
/// The ~2 MB default was written for a database far smaller than a mailbox: at 344 MB
/// it holds a fraction of the b-tree interior pages, so an index seek re-reads them
/// from the OS every time. 8 MiB per connection covers the index levels the hot reads
/// walk, and across the writer plus [`READERS`] it is ~40 MiB — a budget a phone can
/// carry.
const CACHE_KIB: i64 = 8 * 1024;

/// Prepared statements kept compiled per connection.
///
/// The apply path alone uses more than a dozen distinct statements (the object upsert,
/// one per derived-row kind, one delete per derived table), so rusqlite's default of 16
/// would evict on every batch and re-compile the statement it had just dropped. This is
/// comfortably above the store's whole statement set.
const STATEMENT_CACHE: usize = 64;

/// Reader connections opened beside the writer for a file database.
///
/// Enough that a host's concurrent reads — a list read, a thread expansion, a search —
/// overlap each other and a running sync, while the page cache this multiplies
/// ([`CACHE_KIB`]) stays small enough for a phone.
pub(crate) const READERS: usize = 4;

/// Applies the pragmas every connection needs, plus the WAL set a file database adds.
///
/// `execute_batch` tolerates the rows `journal_mode`/`mmap_size` echo back.
pub(crate) fn tune(conn: &Connection, on_disk: bool) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -{CACHE_KIB};
         PRAGMA temp_store = MEMORY;"
    ))
    .map_err(backend)?;
    if on_disk {
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA mmap_size = {MMAP_BYTES};"
        ))
        .map_err(backend)?;
    }
    Ok(())
}

/// Opens the reader connections for a file database, each `query_only`.
pub(crate) fn open_readers(path: &Path) -> Result<Vec<Connection>> {
    (0..READERS)
        .map(|_| {
            let conn = Connection::open(path).map_err(backend)?;
            tune(&conn, true)?;
            conn.execute_batch("PRAGMA query_only = 1;")
                .map_err(backend)?;
            Ok(conn)
        })
        .collect()
}

/// The store's connections: one writer, zero or more readers.
#[derive(Debug)]
pub(crate) struct Pool {
    writer: Mutex<Connection>,
    /// Empty for an in-memory database, where reads fall back to the writer.
    readers: Vec<Mutex<Connection>>,
    /// Where the next reader search starts, so concurrent readers spread out
    /// instead of all contending on the first connection.
    next: AtomicUsize,
}

impl Pool {
    /// Wraps a writer and its readers.
    pub(crate) fn new(writer: Connection, readers: Vec<Connection>) -> Self {
        Self {
            writer: Mutex::new(writer),
            readers: readers.into_iter().map(Mutex::new).collect(),
            next: AtomicUsize::new(0),
        }
    }

    /// The writer, blocking until it is free. Every transaction runs here.
    pub(crate) fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().expect("sqlite writer mutex poisoned")
    }

    /// A free reader — or the writer, when the database is in-memory and there are
    /// none.
    ///
    /// Scans from a rotating start for one that is not in use and blocks on that
    /// start only if every reader is busy, so N concurrent reads use N connections
    /// rather than piling onto one.
    pub(crate) fn reader(&self) -> MutexGuard<'_, Connection> {
        let Some(count) = core::num::NonZeroUsize::new(self.readers.len()) else {
            return self.writer();
        };
        let start = self.next.fetch_add(1, Ordering::Relaxed) % count;
        for offset in 0..count.get() {
            if let Ok(guard) = self.readers[(start + offset) % count].try_lock() {
                return guard;
            }
        }
        self.readers[start]
            .lock()
            .expect("sqlite reader mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::{Pool, READERS};

    fn connection() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().expect("open")
    }

    #[test]
    fn an_unsplit_pool_reads_through_its_writer() {
        // The in-memory case: one connection is the whole database, so a read must
        // still find one rather than deadlock or panic on an empty reader list.
        let pool = Pool::new(connection(), Vec::new());
        let value: i64 = pool
            .reader()
            .query_row("SELECT 1", [], |row| row.get(0))
            .expect("read through the writer");
        assert_eq!(value, 1);
    }

    #[test]
    fn concurrent_readers_land_on_different_connections() {
        // The point of the pool: two reads in flight at once must not serialize.
        // Holding one guard and taking another proves the second did not wait.
        let pool = Pool::new(connection(), (0..READERS).map(|_| connection()).collect());
        let first = pool.reader();
        let second = pool.reader();
        assert!(
            !core::ptr::eq(&raw const *first, &raw const *second),
            "a second reader taken while the first is held must be a different connection"
        );
    }

    #[test]
    fn every_reader_busy_falls_back_to_blocking_on_one() {
        // With all four held, a fifth caller has to wait — but only for a reader,
        // never for the writer, which stays free for the sync that is committing.
        let pool = Pool::new(connection(), (0..READERS).map(|_| connection()).collect());
        let held: Vec<_> = (0..READERS).map(|_| pool.reader()).collect();
        assert_eq!(held.len(), READERS);
        let value: i64 = pool
            .writer()
            .query_row("SELECT 1", [], |row| row.get(0))
            .expect("the writer is not blocked by busy readers");
        assert_eq!(value, 1);
    }
}
