//! The content-addressed filesystem blob area.
//!
//! Raw message sources (and, later, attachments) are Tier-3 content that can be
//! 1–15 MB — far too large to sit in SQLite rows. They live here instead: a
//! directory of files named by the SHA-256 of their bytes, so identical payloads
//! (two IMAP copies of one message) dedupe to one file, names are filesystem-safe
//! and fixed-length, and the relational store keeps only metadata pointing at the
//! hash (`schema.rs` `message_source`). The bytes are sensitive mail data, protected
//! at rest by the host's OS file encryption — the same posture as the database file
//! (`north-star.md`).

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use engine_store::Result;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::convert::backend;

/// The directory holding content-addressed blobs.
///
/// For a file-backed store it sits beside the database and persists; for an
/// in-memory store it is a [`TempDir`] cleaned up when the store drops.
pub(crate) enum BlobArea {
    /// A durable directory beside the database file.
    Persistent(PathBuf),
    /// An ephemeral directory removed on drop (in-memory stores and tests).
    Temporary(TempDir),
}

impl fmt::Debug for BlobArea {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobArea").finish_non_exhaustive()
    }
}

impl BlobArea {
    /// Resolves (creating if absent) the blob directory that sits beside the
    /// database at `db_path` — `<db>.blobs/`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](engine_store::StoreError) if the directory
    /// cannot be created.
    pub(crate) fn beside_db(db_path: &Path) -> Result<Self> {
        let mut name = db_path.file_name().map_or_else(
            || std::ffi::OsString::from("db"),
            std::ffi::OsStr::to_os_string,
        );
        name.push(".blobs");
        let root = db_path.with_file_name(name);
        fs::create_dir_all(&root).map_err(backend)?;
        Ok(Self::Persistent(root))
    }

    /// Creates an ephemeral blob directory, auto-removed when this store drops.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](engine_store::StoreError) if a temp
    /// directory cannot be created.
    pub(crate) fn temporary() -> Result<Self> {
        Ok(Self::Temporary(TempDir::new().map_err(backend)?))
    }

    /// The blob directory root.
    pub(crate) fn root(&self) -> &Path {
        match self {
            Self::Persistent(path) => path,
            Self::Temporary(dir) => dir.path(),
        }
    }
}

/// Writes `bytes` into `<root>/sources/<sha256-hex>.eml` and returns the hex hash
/// naming it. Content-addressed: an identical payload is already present, so the
/// write is skipped; otherwise it is staged in a sibling temp file and atomically
/// renamed into place.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError) on a filesystem
/// failure.
pub(crate) fn write_source(root: &Path, bytes: &[u8]) -> Result<String> {
    let hash = hex(Sha256::digest(bytes).as_slice());
    let dir = root.join("sources");
    fs::create_dir_all(&dir).map_err(backend)?;
    let path = dir.join(format!("{hash}.eml"));
    if !path.exists() {
        let mut tmp = NamedTempFile::new_in(&dir).map_err(backend)?;
        tmp.write_all(bytes).map_err(backend)?;
        tmp.persist(&path).map_err(|err| backend(err.error))?;
    }
    Ok(hash)
}

/// Reads the blob named by `hash`, or `None` if its file is absent (an evicted or
/// externally-removed blob reads as a cache miss).
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError) on a non-`NotFound`
/// filesystem failure.
pub(crate) fn read_source(root: &Path, hash: &str) -> Result<Option<Vec<u8>>> {
    let path = root.join("sources").join(format!("{hash}.eml"));
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(backend(err)),
    }
}

/// Lower-hex encodes bytes (the SHA-256 digest) into a filesystem-safe name.
fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_the_blob_path() {
        // The area's `Debug` is redacted (only the type name), like the store's.
        let area = BlobArea::temporary().unwrap();
        assert_eq!(format!("{area:?}"), "BlobArea { .. }");
    }
}
