//! Contact-photo cache metadata for the content-addressed blob area.

use engine_store::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::convert::backend;

/// One cached photo's metadata row.
pub(crate) struct PhotoRow<'a> {
    pub account: &'a str,
    pub contact: &'a str,
    /// Which media resource on the card this row caches (see the `contact_photo`
    /// table comment) — a card may carry a `PHOTO` and a `LOGO`.
    pub resource: &'a str,
    pub fingerprint: &'a str,
    pub content_hash: &'a str,
    pub media_type: Option<&'a str>,
    pub fetched_at: &'a str,
    /// `true` records that the provider has no photo here; `content_hash` is then
    /// empty and names no blob.
    pub missing: bool,
}

/// One cached photo's metadata, as read back.
pub(crate) struct CachedRow {
    pub content_hash: String,
    pub media_type: Option<String>,
    pub fetched_at: String,
    pub missing: bool,
}

pub(crate) fn upsert(conn: &Connection, row: &PhotoRow<'_>) -> Result<()> {
    let PhotoRow {
        account,
        contact,
        resource,
        fingerprint,
        content_hash,
        media_type,
        fetched_at,
        missing,
    } = *row;
    conn.execute(
        "INSERT INTO contact_photo
         (account, contact, resource, fingerprint, content_hash, media_type, fetched_at, missing)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(account, contact, resource) DO UPDATE SET
             fingerprint = excluded.fingerprint,
             content_hash = excluded.content_hash,
             media_type = excluded.media_type,
             fetched_at = excluded.fetched_at,
             missing = excluded.missing",
        params![
            account,
            contact,
            resource,
            fingerprint,
            content_hash,
            media_type,
            fetched_at,
            i64::from(missing)
        ],
    )
    .map_err(backend)?;
    Ok(())
}

pub(crate) fn select(
    conn: &Connection,
    account: &str,
    contact: &str,
    resource: &str,
    fingerprint: &str,
) -> Result<Option<CachedRow>> {
    conn.query_row(
        "SELECT content_hash, media_type, fetched_at, missing FROM contact_photo
         WHERE account = ?1 AND contact = ?2 AND resource = ?3 AND fingerprint = ?4",
        params![account, contact, resource, fingerprint],
        |row| {
            Ok(CachedRow {
                content_hash: row.get(0)?,
                media_type: row.get(1)?,
                fetched_at: row.get(2)?,
                missing: row.get::<_, i64>(3)? != 0,
            })
        },
    )
    .optional()
    .map_err(backend)
}
