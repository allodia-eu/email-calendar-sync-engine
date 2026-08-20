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

use std::{
    collections::HashSet,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use engine_store::{Result, SweepReport};
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
    write_blob(root, "sources", "eml", bytes)
}

/// Writes contact-photo bytes to the shared content-addressed blob area.
pub(crate) fn write_contact_photo(root: &Path, bytes: &[u8]) -> Result<String> {
    write_blob(root, "contact-photos", "blob", bytes)
}

fn write_blob(root: &Path, namespace: &str, extension: &str, bytes: &[u8]) -> Result<String> {
    let hash = hex(Sha256::digest(bytes).as_slice());
    let dir = root.join(namespace);
    fs::create_dir_all(&dir).map_err(backend)?;
    let path = dir.join(format!("{hash}.{extension}"));
    if !path.exists() {
        let mut tmp = NamedTempFile::new_in(&dir).map_err(backend)?;
        tmp.write_all(bytes).map_err(backend)?;
        tmp.persist(&path).map_err(|err| backend(err.error))?;
    }
    Ok(hash)
}

/// Reads the blob named by `hash` and verifies its contents still hash to `hash`,
/// or `None` if its file is absent **or** fails that check — an evicted, truncated,
/// or corrupted blob reads as a cache miss, so the caller re-fetches rather than
/// serving wrong bytes as a valid body.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError) on a non-`NotFound`
/// filesystem failure.
pub(crate) fn read_source(root: &Path, hash: &str) -> Result<Option<Vec<u8>>> {
    read_blob(root, "sources", "eml", hash)
}

/// Reads and verifies cached contact-photo bytes.
pub(crate) fn read_contact_photo(root: &Path, hash: &str) -> Result<Option<Vec<u8>>> {
    read_blob(root, "contact-photos", "blob", hash)
}

/// Where [`write_contact_photo`] puts (or would put) the blob named by `hash`.
///
/// Names the location without touching the filesystem, for a caller that hands the
/// path to an image decoder instead of reading the bytes itself.
pub(crate) fn contact_photo_path(root: &Path, hash: &str) -> PathBuf {
    root.join("contact-photos").join(format!("{hash}.blob"))
}

fn read_blob(root: &Path, namespace: &str, extension: &str, hash: &str) -> Result<Option<Vec<u8>>> {
    let path = root.join(namespace).join(format!("{hash}.{extension}"));
    match fs::read(&path) {
        Ok(bytes) if hex(Sha256::digest(&bytes).as_slice()) == hash => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(backend(err)),
    }
}

/// The blob namespaces and the extension each writes, in one place so a sweep covers
/// every one of them: a namespace missing here is a namespace that leaks.
pub(crate) const NAMESPACES: [(&str, &str); 2] = [("sources", "eml"), ("contact-photos", "blob")];

/// How old a blob must be before a sweep may remove it.
///
/// A blob is written *before* the row that names it, so a file whose write is still in
/// flight looks unreferenced. Listing before reading the live hashes narrows that window
/// to one directory scan; this is the margin over it. Erring long costs a later reclaim,
/// erring short costs the user a re-download.
const SWEEP_GRACE: Duration = Duration::from_mins(5);

/// One blob file a sweep may remove if no row names its hash.
pub(crate) struct Candidate {
    /// The file's SHA-256 name, matched against the hashes the store still holds.
    pub(crate) hash: String,
    path: PathBuf,
    len: u64,
}

/// Every blob old enough to sweep, across all [`NAMESPACES`].
///
/// Collected **before** the caller reads the live hash set, so a blob written after this
/// listing is not a candidate at all rather than one the grace period has to rescue. A
/// namespace directory that does not exist yet contributes nothing.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError) on a non-`NotFound`
/// filesystem failure.
pub(crate) fn candidates(root: &Path, now: SystemTime) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for (namespace, extension) in NAMESPACES {
        let dir = root.join(namespace);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(backend(err)),
        };
        for entry in entries {
            let path = entry.map_err(backend)?.path();
            // Anything but a `<hash>.<extension>` file is not ours to remove — the
            // staging temp files a concurrent write is using carry no extension.
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some(extension) {
                continue;
            }
            let Some(hash) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            let meta = fs::metadata(&path).map_err(backend)?;
            let young = meta
                .modified()
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_none_or(|age| age < SWEEP_GRACE);
            if young {
                continue;
            }
            out.push(Candidate {
                hash: hash.to_owned(),
                path,
                len: meta.len(),
            });
        }
    }
    Ok(out)
}

/// Removes every candidate whose hash is not in `live`, reporting what that reclaimed.
///
/// A file that vanished between the listing and here (a concurrent sweep, a host
/// clearing its data directory) is not an error — it is already gone.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError) on a non-`NotFound`
/// filesystem failure.
pub(crate) fn remove_unreferenced(
    candidates: &[Candidate],
    live: &HashSet<String>,
) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    for candidate in candidates {
        if live.contains(&candidate.hash) {
            continue;
        }
        match fs::remove_file(&candidate.path) {
            Ok(()) => {
                report.blobs_removed += 1;
                report.bytes_reclaimed += candidate.len;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(backend(err)),
        }
    }
    Ok(report)
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
