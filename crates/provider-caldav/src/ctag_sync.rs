//! Syncing a collection whose server does not do the RFC 6578 `sync-collection` REPORT.
//!
//! Yahoo answers a calendar's `sync-collection` with an empty multistatus and no sync-token, and
//! an address book's with a `400`; it refuses `addressbook-query` with a `400` as well
//! (`docs/spec-deviations.md`, YAHOO-2 to YAHOO-4). What
//! every CalDAV and CardDAV server answers is a `PROPFIND` listing each member with its
//! `getetag`, and a multiget returning their bodies (RFC 4791 §7.9, RFC 6352 §8.7). So such a
//! collection is read in full whenever its CTag (`getctag`, a calendarserver.org extension that
//! changes with any member) moves, and not at all while it stands still. The cursor is the CTag.

use engine_core::sync::SyncState;

use crate::{
    dav::{MultiStatus, has_precondition},
    error::CalDavError,
    multiget::fill_bodies,
    request::{CTAG_PROPFIND, MEMBER_ETAGS_PROPFIND},
    transport::{DavExecutor, DavMethod},
};

/// What a CTag cursor starts with, telling it apart from a server's sync-token.
const PREFIX: &str = "ctag:";

/// Whether `cursor` was minted here rather than by a server's `sync-collection`.
pub(crate) fn is_ctag_cursor(cursor: Option<&SyncState>) -> bool {
    cursor.is_some_and(|cursor| cursor.as_str().starts_with(PREFIX))
}

/// Whether `err`, the answer to a `sync-collection` REPORT, says the server does not do the
/// report: the `DAV:supported-report` precondition in a `403` or `409` (RFC 3253 §3.6), `405` or
/// `501`, or a `400` to the first request of a collection. A `400` to a request carrying a token
/// is not read that way, since a server that never did the report never issued one, and an
/// expired token is answered with RFC 6578's own precondition.
pub(crate) fn refuses_sync_collection(err: &CalDavError, first: bool) -> bool {
    match err {
        CalDavError::Status { status, body, .. } => {
            matches!(status, 405 | 501)
                || (first && *status == 400)
                || (matches!(status, 403 | 409) && has_precondition(body, "supported-report"))
        }
        _ => false,
    }
}

/// A collection read through its CTag.
pub(crate) enum CtagSync {
    /// The CTag is the one `cursor` holds: nothing changed since.
    Unchanged(SyncState),
    /// The collection's listing, each member with its `data` property where the server returned
    /// one, and the cursor to hold next.
    Changed {
        members: MultiStatus,
        next: SyncState,
    },
}

/// Reads `collection` through its CTag, fetching each member's `data` with `multiget` when the
/// CTag differs from the one `cursor` holds.
///
/// # Errors
///
/// Returns [`CalDavError`] on a failed request, or when the collection has no CTag either, so
/// there is nothing to sync it by.
pub(crate) async fn ctag_sync(
    exec: &dyn DavExecutor,
    collection: &str,
    cursor: Option<&SyncState>,
    data: &str,
    multiget: fn(&[String]) -> String,
) -> Result<CtagSync, CalDavError> {
    let properties = exec
        .send(DavMethod::Propfind, collection, "0", CTAG_PROPFIND.into())
        .await?
        .into_multistatus()?;
    let ctag = properties
        .responses
        .iter()
        .find_map(|response| response.props.get("getctag"))
        .ok_or_else(|| {
            CalDavError::protocol("the collection offers neither sync-collection nor a CTag")
        })?;
    let next = SyncState::new(format!("{PREFIX}{ctag}"));
    if cursor.is_some_and(|cursor| cursor.as_str() == next.as_str()) {
        return Ok(CtagSync::Unchanged(next));
    }
    let mut members = exec
        .send(
            DavMethod::Propfind,
            collection,
            "1",
            MEMBER_ETAGS_PROPFIND.into(),
        )
        .await?
        .into_multistatus()?;
    fill_bodies(exec, collection, &mut members, data, multiget).await?;
    Ok(CtagSync::Changed { members, next })
}
