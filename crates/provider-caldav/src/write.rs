//! CalDAV calendar writes: conditional `PUT` (create/update) and `DELETE`.
//!
//! A calendar object resource is created or replaced with a single `PUT` of its
//! iCalendar body (RFC 4791 §5.3.2) and removed with `DELETE`. Optimistic
//! concurrency rides on the resource `ETag`: a create sends `If-None-Match: *`
//! (never overwrite), an update or guarded delete sends `If-Match: <etag>` (only
//! while unchanged). A failed precondition is `412` → [`FailureClass::Conflict`],
//! recovered by refetch, never a blind retry (`error.rs`). The body is the caller's
//! round-tripped [`RawIcal`](engine_core::raw::RawIcal), never a re-serialized
//! projection (`calendar-semantics.md`).
//!
//! [`FailureClass::Conflict`]: engine_core::error::FailureClass::Conflict

use engine_core::version::ETag;
use engine_provider::{EventDeletion, EventWrite, EventWriteReceipt, WritePrecondition};

use crate::error::CalDavError;
use crate::transport::{DavExecutor, DavMethod, Precondition, WriteRequest};

/// The iCalendar media type sent on a `PUT` (RFC 5545 §3.1; RFC 4791 §5.3.2).
const ICALENDAR_CONTENT_TYPE: &str = "text/calendar; charset=utf-8";

/// `PUT`s a calendar object resource, returning a receipt with the server's new
/// `ETag` when it supplied one (else `None` — the next sync learns the revision).
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure; a precondition failure is a
/// `412` classified [`Conflict`](engine_core::error::FailureClass::Conflict).
pub(crate) async fn put_event(
    exec: &dyn DavExecutor,
    write: &EventWrite,
) -> Result<EventWriteReceipt, CalDavError> {
    let request = WriteRequest {
        method: DavMethod::Put,
        href: write.href.as_str().to_owned(),
        content_type: Some(ICALENDAR_CONTENT_TYPE),
        precondition: precondition(&write.precondition),
        body: write.ical.as_str().to_owned(),
    };
    let etag = exec.send_write(request).await?.into_write_etag()?;
    Ok(EventWriteReceipt::new(
        write.href.key().clone(),
        write.uid.clone(),
        etag.map(ETag::new),
    ))
}

/// `DELETE`s a calendar object resource, guarded by `If-Match` when `deletion`
/// carries an ETag.
///
/// `DELETE` is idempotent (RFC 7231 §4.3.5): a resource that is **already absent**
/// (`404`/`410`) means the desired end state already holds, so it resolves as
/// success — not the `Permanent` error a generic non-`2xx` check would yield. This
/// is what makes the outbox's "a recovery retry is safe" promise true for deletes:
/// re-running a delete whose response was lost (the first one landed) sees `404` and
/// succeeds. A `412` (the resource still exists but its ETag moved) is a genuine
/// `If-Match` [`Conflict`](engine_core::error::FailureClass::Conflict), surfaced for
/// refetch.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure; a failed `If-Match` is a
/// `412` classified [`Conflict`](engine_core::error::FailureClass::Conflict).
pub(crate) async fn delete_event(
    exec: &dyn DavExecutor,
    deletion: &EventDeletion,
) -> Result<(), CalDavError> {
    let precondition = deletion.etag.as_ref().map_or(Precondition::None, |etag| {
        Precondition::IfMatch(etag.as_str().to_owned())
    });
    let request = WriteRequest {
        method: DavMethod::Delete,
        href: deletion.href.as_str().to_owned(),
        content_type: None,
        precondition,
        body: String::new(),
    };
    let response = exec.send_write(request).await?;
    // Already-gone is success for an idempotent delete; anything else non-`2xx`
    // (incl. a `412` If-Match conflict) flows through the classified error path.
    if matches!(response.status, 404 | 410) {
        return Ok(());
    }
    response.into_write_etag()?;
    Ok(())
}

/// Translates the engine's [`WritePrecondition`] into the transport precondition.
fn precondition(precondition: &WritePrecondition) -> Precondition {
    match precondition {
        WritePrecondition::IfNoneMatch => Precondition::IfNoneMatch,
        WritePrecondition::IfMatch(etag) => Precondition::IfMatch(etag.as_str().to_owned()),
        WritePrecondition::Unconditional => Precondition::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::error::FailureClass;
    use engine_core::ids::{EventId, Uid};
    use engine_core::raw::RawIcal;

    use crate::test_support::{Replay, wrote};

    fn href() -> EventId {
        EventId::try_from("/dav/cal/alice%40test.local/default/evt-1.ics").unwrap()
    }

    fn uid() -> Uid {
        Uid::new("evt-1@test.local").unwrap()
    }

    const BODY: &str =
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:evt-1@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR";

    #[tokio::test]
    async fn create_puts_with_if_none_match_and_returns_the_new_etag() {
        let exec = Replay::new(vec![wrote(201, Some("\"v1\""))]);
        let write = EventWrite::create(href(), uid(), RawIcal::new(BODY));
        let receipt = put_event(&exec, &write).await.unwrap();

        // The receipt carries the resource key, the uid, and the server's new ETag.
        assert_eq!(receipt.event_key, href().key().clone());
        assert_eq!(receipt.uid, uid());
        assert_eq!(receipt.etag, Some(ETag::new("\"v1\"")));

        // The executor saw a PUT to the href with a text/calendar body and the
        // create precondition.
        let writes = exec.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].method, DavMethod::Put);
        assert_eq!(writes[0].href, href().as_str());
        assert_eq!(writes[0].content_type, Some(ICALENDAR_CONTENT_TYPE));
        assert_eq!(writes[0].precondition, Precondition::IfNoneMatch);
        assert!(writes[0].body.contains("UID:evt-1@test.local"));
    }

    #[tokio::test]
    async fn update_puts_with_if_match() {
        let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
        let write = EventWrite::update(href(), uid(), RawIcal::new(BODY), ETag::new("\"v1\""));
        let receipt = put_event(&exec, &write).await.unwrap();
        assert_eq!(receipt.etag, Some(ETag::new("\"v2\"")));
        assert_eq!(
            exec.writes()[0].precondition,
            Precondition::IfMatch("\"v1\"".to_owned())
        );
    }

    #[tokio::test]
    async fn a_put_without_a_response_etag_yields_no_etag() {
        // Some servers omit the ETag on the PUT; the receipt then has None and the
        // caller learns the revision from the next sync.
        let exec = Replay::new(vec![wrote(201, None)]);
        let write = EventWrite::create(href(), uid(), RawIcal::new(BODY));
        let receipt = put_event(&exec, &write).await.unwrap();
        assert_eq!(receipt.etag, None);
    }

    #[tokio::test]
    async fn a_precondition_failure_is_a_conflict() {
        let exec = Replay::new(vec![wrote(412, None)]);
        let write = EventWrite::update(href(), uid(), RawIcal::new(BODY), ETag::new("\"stale\""));
        let err = put_event(&exec, &write).await.unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }

    #[tokio::test]
    async fn delete_sends_if_match_when_guarded() {
        let exec = Replay::new(vec![wrote(204, None)]);
        let deletion = EventDeletion::if_match(href(), ETag::new("\"v2\""));
        delete_event(&exec, &deletion).await.unwrap();
        let writes = exec.writes();
        assert_eq!(writes[0].method, DavMethod::Delete);
        assert_eq!(writes[0].href, href().as_str());
        assert!(writes[0].body.is_empty());
        assert_eq!(
            writes[0].precondition,
            Precondition::IfMatch("\"v2\"".to_owned())
        );
    }

    #[tokio::test]
    async fn unconditional_delete_sends_no_precondition() {
        let exec = Replay::new(vec![wrote(204, None)]);
        let deletion = EventDeletion::unconditional(href());
        delete_event(&exec, &deletion).await.unwrap();
        assert_eq!(exec.writes()[0].precondition, Precondition::None);
    }

    #[tokio::test]
    async fn deleting_an_already_gone_resource_is_idempotent_success() {
        // DELETE is idempotent (RFC 7231 §4.3.5): an already-absent resource
        // (404/410) resolves as success, so a lost-ack retry of a landed delete
        // does not report a hard failure.
        for status in [404, 410] {
            let exec = Replay::new(vec![wrote(status, None)]);
            let deletion = EventDeletion::unconditional(href());
            delete_event(&exec, &deletion).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_delete_if_match_conflict_is_surfaced() {
        // A 412 (the resource still exists but its ETag moved) is a real conflict,
        // distinct from the already-gone case — the caller refetches and merges.
        let exec = Replay::new(vec![wrote(412, None)]);
        let deletion = EventDeletion::if_match(href(), ETag::new("\"stale\""));
        let err = delete_event(&exec, &deletion).await.unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }

    #[tokio::test]
    async fn a_delete_server_error_still_surfaces() {
        // A genuine failure (e.g. 503) is not swallowed by the idempotent-gone path.
        let exec = Replay::new(vec![wrote(503, None)]);
        let deletion = EventDeletion::unconditional(href());
        let err = delete_event(&exec, &deletion).await.unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Retryable);
    }
}
