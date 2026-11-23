//! What an import's collection, `PUT` and `UID` lookup answer, for calendars and address books
//! alike.
//!
//! An event and a card are imported the same way: look the `UID` up, then `PUT` with
//! `If-None-Match: *` to the name the `UID` gives. The two protocols answer with the same codes
//! and the same `no-uid-conflict` precondition (RFC 4791 §5.3.2.1, RFC 6352 §6.3.2.1), and state
//! their size limit and contents marker on the collection the same way, so each answer is read
//! here once. Whether a refusal is the object's or the target's, and whether a failure is a
//! throttle, is the one write classifier's (`CalDavError::write_refusal`, `failure_class`).

use std::num::NonZeroU64;

use engine_core::{error::FailureClass, sync::SyncState};
use engine_provider::{ProviderError, ProviderResult};

use crate::{
    ctag_sync::ctag_cursor,
    dav::{MultiStatus, has_precondition},
    error::CalDavError,
    precondition::precondition_href,
    response::HttpResponse,
    transport::{DavExecutor, DavMethod},
};

/// What a `PUT` with `If-None-Match: *` came back as.
#[derive(Debug)]
pub(crate) enum PutAnswer {
    /// Stored, with the `ETag` the server reported.
    Stored(Option<String>),
    /// Our own name is taken (a bare `412`): by an earlier attempt of this import, or by an
    /// object that has nothing to do with it, which [`name_held_by`] tells apart.
    NameTaken,
    /// The server refused a second object with the `UID`, naming the holder where it did.
    HeldBy(Option<String>),
    /// Anything else, classified as a write's answer (`CalDavError::after_write`): turn it
    /// into the caller's error with `CalDavError::refused_write`.
    Failed(CalDavError),
}

/// Reads `response`, the answer to an import's `PUT`.
pub(crate) fn read_put(response: HttpResponse) -> PutAnswer {
    if (200..300).contains(&response.status) {
        return PutAnswer::Stored(response.etag);
    }
    if matches!(response.status, 403 | 409) && has_precondition(&response.body, "no-uid-conflict") {
        return PutAnswer::HeldBy(precondition_href(&response.body, "no-uid-conflict"));
    }
    let failed = CalDavError::status(response.status, response.body)
        .with_retry_after(response.retry_after)
        .after_write();
    if response.status == 412 && failed.failure_class() == FailureClass::Conflict {
        return PutAnswer::NameTaken;
    }
    PutAnswer::Failed(failed)
}

/// Whether the object at `href` is the one `holds` recognises (by its `UID`): read after a bare
/// `412`, so an earlier attempt of this import is linked and a stranger at the name is not.
///
/// # Errors
///
/// A classified [`ProviderError`] when the object cannot be read, and
/// [`FailureClass::Conflict`] when another object holds the name.
pub(crate) async fn name_held_by(
    exec: &dyn DavExecutor,
    href: &str,
    holds: impl Fn(&str) -> bool,
) -> ProviderResult<()> {
    let response = exec
        .send(DavMethod::Get, href, "0", String::new())
        .await
        .map_err(|err| err.content_free("reading the name an import found taken"))?;
    if !(200..300).contains(&response.status) {
        return Err(CalDavError::status(response.status, response.body)
            .with_retry_after(response.retry_after)
            .content_free("reading the name an import found taken"));
    }
    if holds(&response.body) {
        Ok(())
    } else {
        Err(ProviderError::conflict(
            "importing: another object already has the name this one's UID gives",
        ))
    }
}

/// The href of the object in `collection` that `query` finds and `holds` accepts, by its `data`
/// property (`calendar-data`, `address-data`).
///
/// A server that answers the query with a refusal (a `Permanent` class: the report not supported,
/// a collection it will not search) is not asked again: the `PUT` goes ahead, and the server's own
/// refusal of a second `UID` is what remains. A throttle, a credential refused, or a server error
/// answers the object, since an unanswered lookup is not one that found nothing.
pub(crate) async fn holder(
    exec: &dyn DavExecutor,
    collection: &str,
    query: String,
    data: &str,
    holds: impl Fn(&str) -> bool,
) -> ProviderResult<Option<String>> {
    let response = exec
        .send(DavMethod::Report, collection, "1", query)
        .await
        .map_err(|err| err.content_free("looking up a UID"))?;
    if response.status != 207 {
        let failed = CalDavError::status(response.status, response.body)
            .with_retry_after(response.retry_after);
        return match failed.failure_class() {
            FailureClass::Permanent => Ok(None),
            _ => Err(failed.content_free("looking up a UID")),
        };
    }
    let found = response
        .into_multistatus()
        .map_err(|err| err.content_free("looking up a UID"))?;
    Ok(found
        .responses
        .iter()
        .find(|object| object.props.get(data).is_some_and(&holds))
        .map(|object| object.href().to_owned()))
}

/// The import cursor a collection's `PROPFIND` names: its `sync-token`, which a
/// `sync-collection` delta starts from, or its CTag where it has none, in the form
/// [`crate::ctag_sync`] reads.
pub(crate) fn cursor_of(props: &MultiStatus) -> Option<SyncState> {
    let first = props.responses.first()?;
    let value = |name| first.props.get(name).filter(|value| !value.is_empty());
    value("sync-token")
        .map(SyncState::new)
        .or_else(|| value("getctag").map(ctag_cursor))
}

/// The largest object a collection's `PROPFIND` says it takes (`max-resource-size`, RFC 4791
/// §5.2.5, RFC 6352 §6.2.3).
pub(crate) fn max_size(props: &MultiStatus) -> Option<NonZeroU64> {
    props
        .responses
        .first()?
        .props
        .get("max-resource-size")?
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(NonZeroU64::new)
}

/// The failure in `answer` that ends a batch, if it is one: a throttle or a refused credential,
/// which every later object would meet too (`providers.md` → "Import"). The rest of the batch is
/// then answered [`not_sent`](engine_provider::import::not_sent) without a request.
pub(crate) fn ending<T>(answer: &ProviderResult<T>) -> Option<&ProviderError> {
    answer.as_ref().err().filter(|failed| {
        matches!(
            failed.class(),
            FailureClass::RateLimited | FailureClass::Authentication
        )
    })
}
