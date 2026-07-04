//! CalDAV event sync via the `sync-collection` REPORT (RFC 6578).
//!
//! One REPORT is the whole primitive: with an **empty** prior token it returns
//! every resource (a snapshot whose `present` set tombstones anything else); with
//! a held token it returns only the changed (`2xx`, carrying `calendar-data`) and
//! removed (`404`) resources (a delta). Either way the response carries the next
//! `sync-token`, which becomes the cursor. A server that rejects a stale token
//! (RFC 6578 §3.2 `valid-sync-token`) is recovered by re-running as a snapshot,
//! inside the adapter — the same self-healing the JMAP adapter does for
//! `cannotCalculateChanges`. A single malformed resource is skipped, never failing
//! the whole pass (`calendar-semantics.md`).

use std::collections::BTreeSet;

use engine_core::calendar::Event;
use engine_core::error::FailureClass;
use engine_core::ids::{CalendarId, EventId, ProviderKey};
use engine_core::sync::{SyncState, SyncUpdate};
use engine_core::version::{ETag, RevisionTokens};
use engine_provider::ScopeSync;

use crate::dav::MultiStatus;
use crate::error::CalDavError;
use crate::ical::parse_calendar_object;
use crate::request::sync_collection_report;
use crate::transport::{DavExecutor, DavMethod};

/// Syncs the events of one calendar collection since `cursor`.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure or a malformed response.
pub(crate) async fn sync_events(
    exec: &dyn DavExecutor,
    collection_href: &str,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
) -> Result<ScopeSync<Event>, CalDavError> {
    let token = cursor.map_or("", SyncState::as_str);
    let mut snapshot = token.is_empty();
    let multistatus = match report(exec, collection_href, token).await {
        Ok(multistatus) => multistatus,
        // A rejected sync-token forces a full re-read (RFC 6578 §3.2).
        Err(err) if !snapshot && err.failure_class() == FailureClass::NeedsResync => {
            snapshot = true;
            report(exec, collection_href, "").await?
        }
        Err(err) => return Err(err),
    };

    // RFC 6578 §3.6: a sync-collection response MUST carry the next sync-token.
    // A missing token is a protocol error — never silently coerced to "" (which
    // would force a full re-snapshot every pass, masking the violation).
    let next_cursor = multistatus
        .sync_token
        .clone()
        .map(SyncState::new)
        .ok_or_else(|| CalDavError::protocol("sync-collection response had no sync-token"))?;
    let update = build_update(&multistatus, calendar, snapshot);
    Ok(ScopeSync::new(update, next_cursor))
}

/// Issues one `sync-collection` REPORT and parses the multistatus body.
async fn report(
    exec: &dyn DavExecutor,
    collection_href: &str,
    token: &str,
) -> Result<MultiStatus, CalDavError> {
    exec.send(
        DavMethod::Report,
        collection_href,
        "1",
        sync_collection_report(token),
    )
    .await?
    .into_multistatus()
}

/// Turns the REPORT's responses into a snapshot or delta [`SyncUpdate`].
///
/// Infallible: a malformed or empty-href resource is skipped (never failing the
/// whole pass), consistent with the per-resource degrade elsewhere.
fn build_update(
    multistatus: &MultiStatus,
    calendar: &CalendarId,
    snapshot: bool,
) -> SyncUpdate<Event> {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut present = BTreeSet::new();
    for response in &multistatus.responses {
        if response.is_removed() {
            // A removal may cover several hrefs (RFC 4918 §14.16); tombstone each.
            // A bad/empty href is skipped, never failing the whole pass.
            removed.extend(response.hrefs.iter().filter_map(|href| href_key(href)));
            continue;
        }
        // A response with no calendar-data is the collection itself, not a
        // resource — skip it.
        let Some(data) = response.props.get("calendar-data") else {
            continue;
        };
        // A single malformed/empty-href resource is skipped, never failing the
        // whole pass (consistent with the malformed-iCal skip below).
        let Ok(id) = EventId::try_from(response.href()) else {
            continue;
        };
        let Ok(mut event) = parse_calendar_object(data, id, calendar.clone()) else {
            continue;
        };
        if let Some(etag) = response.props.get("getetag") {
            event.revisions = RevisionTokens::from_etag(ETag::new(etag));
        }
        present.insert(event.id.key().clone());
        changed.push(event);
    }
    if snapshot {
        SyncUpdate::snapshot(changed, present)
    } else {
        SyncUpdate::delta(changed, removed)
    }
}

/// The provider key for a resource href (the same key its [`EventId`] wraps), or
/// `None` for an empty/invalid href (skipped rather than failing the pass).
fn href_key(href: &str) -> Option<ProviderKey> {
    ProviderKey::new(href).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Replay, ok};
    use crate::transport::HttpResponse;

    fn replay(responses: Vec<HttpResponse>) -> Replay {
        Replay::new(responses)
    }

    fn calendar() -> CalendarId {
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap()
    }

    #[tokio::test]
    async fn initial_sync_is_a_snapshot_of_every_resource() {
        let exec = replay(vec![ok(include_str!("../tests/fixtures/sync-initial.xml"))]);
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            None,
        )
        .await
        .unwrap();
        assert!(result.is_snapshot());
        assert_eq!(result.next_cursor.as_str(), "urn:stalwart:davsync:16");
        let SyncUpdate::Snapshot { objects, present } = &result.update else {
            panic!("expected a snapshot");
        };
        // Six resources (the collection self-response is skipped).
        assert_eq!(objects.len(), 6);
        assert_eq!(present.len(), 6);
        // Each event carries its ETag and is keyed by its href.
        let oneoff = objects
            .iter()
            .find(|e| e.uid.as_str() == "oneoff-2001@test.local")
            .unwrap();
        assert!(oneoff.revisions.etag.is_some());
        assert!(oneoff.id.as_str().ends_with("oneoff-2001.ics"));
    }

    #[tokio::test]
    async fn held_token_yields_an_empty_delta() {
        let exec = replay(vec![ok(include_str!("../tests/fixtures/sync-noop.xml"))]);
        let cursor = SyncState::new("urn:stalwart:davsync:16");
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            Some(&cursor),
        )
        .await
        .unwrap();
        assert!(!result.is_snapshot());
        let SyncUpdate::Delta { changed, removed } = &result.update else {
            panic!("expected a delta");
        };
        assert!(changed.is_empty());
        assert!(removed.is_empty());
    }

    #[tokio::test]
    async fn delta_reports_changed_and_removed_resources() {
        let body = "<D:multistatus xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/dav/cal/alice%40test.local/default/oneoff-2001.ics</D:href><D:propstat><D:prop><D:getetag>\"v2\"</D:getetag><A:calendar-data><![CDATA[BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:oneoff-2001@test.local\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\nDTEND;TZID=Europe/Amsterdam:20260318T120000\nSUMMARY:Rescheduled\nEND:VEVENT\nEND:VCALENDAR]]></A:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/cal/alice%40test.local/default/gone.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>urn:stalwart:davsync:17</D:sync-token></D:multistatus>";
        let exec = replay(vec![ok(body)]);
        let cursor = SyncState::new("urn:stalwart:davsync:16");
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            Some(&cursor),
        )
        .await
        .unwrap();
        let SyncUpdate::Delta { changed, removed } = &result.update else {
            panic!("expected a delta");
        };
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].title, "Rescheduled");
        assert_eq!(removed.len(), 1);
        assert!(removed[0].as_str().ends_with("gone.ics"));
        assert_eq!(result.next_cursor.as_str(), "urn:stalwart:davsync:17");
    }

    #[tokio::test]
    async fn a_rejected_token_recovers_with_a_snapshot() {
        let rejected = HttpResponse {
            status: 409,
            body: "<D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>".to_owned(),
            location: None,
            etag: None,
        };
        let exec = replay(vec![
            rejected,
            ok(include_str!("../tests/fixtures/sync-initial.xml")),
        ]);
        let cursor = SyncState::new("stale-token");
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            Some(&cursor),
        )
        .await
        .unwrap();
        // The stale delta became a full snapshot, so stale rows get tombstoned.
        assert!(result.is_snapshot());
    }

    #[tokio::test]
    async fn a_response_missing_the_sync_token_is_an_error() {
        // RFC 6578 requires the next sync-token; omitting it must error, not
        // silently reset the cursor to "" (which would force perpetual snapshots).
        let body = "<D:multistatus xmlns:D=\"DAV:\"></D:multistatus>";
        let exec = replay(vec![ok(body)]);
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            None,
        )
        .await;
        assert!(matches!(result, Err(CalDavError::Protocol(_))));
    }

    #[tokio::test]
    async fn a_resource_with_an_empty_href_is_skipped_not_fatal() {
        // One response carries calendar-data but no <href> (malformed); a valid
        // resource follows. The bad one is skipped, the valid one synced — the
        // whole pass must not fail.
        let body = "<D:multistatus xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:propstat><D:prop><A:calendar-data><![CDATA[BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:nohref@x\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\nEND:VEVENT\nEND:VCALENDAR]]></A:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/cal/alice%40test.local/default/ok.ics</D:href><D:propstat><D:prop><A:calendar-data><![CDATA[BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:ok@x\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\nSUMMARY:Kept\nEND:VEVENT\nEND:VCALENDAR]]></A:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>urn:stalwart:davsync:18</D:sync-token></D:multistatus>";
        let exec = replay(vec![ok(body)]);
        let result = sync_events(
            &exec,
            "/dav/cal/alice%40test.local/default/",
            &calendar(),
            None,
        )
        .await
        .unwrap();
        let SyncUpdate::Snapshot { objects, .. } = &result.update else {
            panic!("expected a snapshot");
        };
        assert_eq!(objects.len(), 1, "the empty-href resource is skipped");
        assert_eq!(objects[0].title, "Kept");
    }
}
