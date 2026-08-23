//! Unit tests for the CalDAV write layer: the request shape each verb sends (method,
//! precondition, body) and the response→receipt/error mapping, driven through the fake
//! `DavExecutor`. A sibling file so `write.rs` stays under the line limit.

use engine_core::{
    error::FailureClass,
    ids::{CalendarId, EventId, ProviderKey, Uid},
    membership::Memberships,
    raw::RawIcal,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_provider::{EventPatch, Occurrence, PatchTarget};

use super::*;
use crate::test_support::{Replay, wrote};

fn href() -> EventId {
    EventId::try_from("/dav/cal/alice%40test.local/default/evt-1.ics").unwrap()
}

fn uid() -> Uid {
    Uid::new("evt-1@test.local").unwrap()
}

fn calendar() -> CalendarId {
    CalendarId::new(ProviderKey::new("/dav/cal/alice%40test.local/default/").unwrap())
}

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 20, 8, 0, 0).unwrap()
}

fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(format!("2026-06-25T{hour:02}:00:00").parse().unwrap())
}

const BODY: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:evt-1@test.local\r\n\
                    DTSTAMP:20260601T000000Z\r\nDTSTART:20260625T140000Z\r\n\
                    DTEND:20260625T150000Z\r\nSUMMARY:Old\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// An event as `sync_events` hands it back: the stored raw plus the ETag it was read at.
fn stored(etag: Option<&str>) -> Event {
    let mut event = Event::new(href(), uid(), Memberships::of_one(calendar()), at(14));
    event.raw_ical = Some(RawIcal::new(BODY));
    event.revisions = etag.map_or_else(RevisionTokens::none, |e| {
        RevisionTokens::from_etag(ETag::new(e))
    });
    event
}

/// The same event as a weekly series — the only shape an occurrence can be removed from.
const SERIES: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:evt-1@test.local\r\n\
                      DTSTAMP:20260601T000000Z\r\nDTSTART:20260625T140000Z\r\n\
                      DTEND:20260625T150000Z\r\nRRULE:FREQ=WEEKLY\r\n\
                      SUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn series(etag: Option<&str>) -> Event {
    let mut event = stored(etag);
    event.raw_ical = Some(RawIcal::new(SERIES));
    event
}

fn draft() -> EventDraft {
    EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        at(14),
        at(15),
        stamp(),
    )
}

#[tokio::test]
async fn create_puts_the_built_document_with_if_none_match() {
    let exec = Replay::new(vec![wrote(201, Some("\"v1\""))]);
    let receipt = create_event(&exec, href(), &draft()).await.unwrap();

    // The receipt names the href the create resolved to, the uid, and the new ETag.
    assert_eq!(receipt.event, href());
    assert_eq!(receipt.uid, uid());
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v1\"")));

    // The executor saw a PUT of a *built* iCalendar document under the create
    // precondition — the caller never handed us bytes.
    let writes = exec.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, DavMethod::Put);
    assert_eq!(writes[0].href, href().as_str());
    assert_eq!(writes[0].content_type, Some(ICALENDAR_CONTENT_TYPE));
    assert_eq!(writes[0].precondition, Precondition::IfNoneMatch);
    assert!(writes[0].body.contains("UID:evt-1@test.local"));
    assert!(writes[0].body.contains("SUMMARY:Sprint planning"));
}

#[tokio::test]
async fn patch_puts_the_stored_document_edited_in_place_under_if_match() {
    let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
    let base = stored(Some("\"v1\""));
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let receipt = patch_event(&exec, &base, &edit).await.unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v2\"")));

    let writes = exec.writes();
    // The guard is the revision the caller read, taken off the base — never
    // hand-assembled, so it cannot be stale by construction.
    assert_eq!(
        writes[0].precondition,
        Precondition::IfMatch("\"v1\"".to_owned())
    );
    // The body is the *stored* document with only the summary rewritten: the
    // untouched DTSTART is still there byte-for-byte.
    assert!(writes[0].body.contains("SUMMARY:Renamed\r\n"));
    assert!(!writes[0].body.contains("SUMMARY:Old"));
    assert!(writes[0].body.contains("DTSTART:20260625T140000Z\r\n"));
}

#[tokio::test]
async fn patching_an_event_we_never_synced_is_refused_not_rebuilt() {
    // No stored raw means we do not hold the document. Rebuilding it from the lossy
    // projection would silently drop the VALARMs, the VTIMEZONE, the X- properties —
    // a save that looks like it worked. Refuse instead, and say why.
    let exec = Replay::new(vec![]);
    let mut base = stored(Some("\"v1\""));
    base.raw_ical = None;
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let err = patch_event(&exec, &base, &edit).await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(exec.writes().is_empty(), "nothing may reach the network");
}

#[tokio::test]
async fn a_put_without_a_response_etag_yields_no_revision() {
    // Some servers omit the ETag on the PUT; the receipt then carries no revision and
    // the caller learns it from the next sync.
    let exec = Replay::new(vec![wrote(201, None)]);
    let receipt = create_event(&exec, href(), &draft()).await.unwrap();
    assert!(receipt.revisions.is_empty());
}

#[tokio::test]
async fn a_stale_guard_is_a_conflict() {
    let exec = Replay::new(vec![wrote(412, None)]);
    let base = stored(Some("\"stale\""));
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let err = patch_event(&exec, &base, &edit).await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_document_write_replaces_the_bytes_under_the_bases_guard() {
    // The iMIP RSVP path: the caller assembled the document itself.
    let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
    let base = stored(Some("\"v1\""));
    let write = EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"));
    let receipt = put_event(&exec, &write).await.unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v2\"")));

    let writes = exec.writes();
    assert_eq!(
        writes[0].precondition,
        Precondition::IfMatch("\"v1\"".to_owned())
    );
    assert_eq!(writes[0].body, "BEGIN:VCALENDAR\r\nEND:VCALENDAR");
}

#[tokio::test]
async fn a_document_create_sends_if_none_match_rather_than_replacing_blindly() {
    // Storing an invitation that arrived as mail: the caller mints the href and hands
    // over the invitation's own VEVENT, ORGANIZER, ATTENDEE and SEQUENCE intact — and
    // asks the server to refuse if anything is already there, so a copy the server
    // scheduled a moment ago is a `Conflict` rather than an overwrite nobody sees.
    let exec = Replay::new(vec![wrote(201, Some("\"v1\""))]);
    let invitation = RawIcal::new(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:m@t\r\nORGANIZER:mailto:boss@test.local\r\n\
         ATTENDEE:mailto:me@test.local\r\nSEQUENCE:3\r\nEND:VEVENT\r\nEND:VCALENDAR",
    );
    let write = EventWrite::creating(href(), uid(), invitation.clone());
    let receipt = put_event(&exec, &write).await.unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v1\"")));

    let writes = exec.writes();
    assert_eq!(writes[0].method, DavMethod::Put);
    assert_eq!(writes[0].precondition, Precondition::IfNoneMatch);
    assert_eq!(writes[0].body, invitation.as_str());
}

#[tokio::test]
async fn a_document_create_onto_an_occupied_href_is_a_conflict() {
    // The precondition biting: `412` is the whole reason the create state exists, so it
    // must reach the caller classified for a refetch and never as a blind retry.
    let exec = Replay::new(vec![wrote(412, None)]);
    let write = EventWrite::creating(
        href(),
        uid(),
        RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
    );
    let err = put_event(&exec, &write).await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}

#[tokio::test]
async fn an_unconditional_document_write_sends_no_precondition() {
    let exec = Replay::new(vec![wrote(204, None)]);
    let write = EventWrite::unconditional(
        href(),
        uid(),
        RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
    );
    put_event(&exec, &write).await.unwrap();
    assert_eq!(exec.writes()[0].precondition, Precondition::None);
}

#[tokio::test]
async fn delete_sends_if_match_when_guarded() {
    let exec = Replay::new(vec![wrote(204, None)]);
    let base = stored(Some("\"v2\""));
    delete_event(&exec, None, &EventDeletion::of(&base))
        .await
        .unwrap();
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
    delete_event(&exec, None, &EventDeletion::unconditional(href(), uid()))
        .await
        .unwrap();
    assert_eq!(exec.writes()[0].precondition, Precondition::None);
}

#[tokio::test]
async fn a_guard_naming_no_revision_degrades_to_unconditional() {
    // An event assembled by hand (never synced) carries no ETag, so there is nothing to
    // put in an `If-Match`. Rejecting the write on that technicality would be worse than
    // sending it unguarded — and `calendar_write_guard` is what tells the host what a
    // guard is worth on this transport.
    let exec = Replay::new(vec![wrote(204, None)]);
    let base = stored(None);
    delete_event(&exec, None, &EventDeletion::of(&base))
        .await
        .unwrap();
    assert_eq!(exec.writes()[0].precondition, Precondition::None);
}

#[tokio::test]
async fn deleting_an_already_gone_resource_is_idempotent_success() {
    // DELETE is idempotent (RFC 7231 §4.3.5): an already-absent resource
    // (404/410) resolves as success, so a lost-ack retry of a landed delete
    // does not report a hard failure.
    for status in [404, 410] {
        let exec = Replay::new(vec![wrote(status, None)]);
        delete_event(&exec, None, &EventDeletion::unconditional(href(), uid()))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_delete_if_match_conflict_is_surfaced() {
    // A 412 (the resource still exists but its ETag moved) is a real conflict,
    // distinct from the already-gone case — the caller refetches and merges.
    let exec = Replay::new(vec![wrote(412, None)]);
    let base = stored(Some("\"stale\""));
    let err = delete_event(&exec, None, &EventDeletion::of(&base))
        .await
        .unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_delete_server_error_still_surfaces() {
    // A genuine failure (e.g. 503) is not swallowed by the idempotent-gone path.
    let exec = Replay::new(vec![wrote(503, None)]);
    let err = delete_event(&exec, None, &EventDeletion::unconditional(href(), uid()))
        .await
        .unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Retryable);
}

#[tokio::test]
async fn removing_one_occurrence_puts_the_series_back_with_an_exdate() {
    // There is no per-occurrence resource to DELETE here, so the verb changes shape
    // entirely: a guarded PUT of the series, with the occurrence excluded from it.
    let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
    let base = series(Some("\"v1\""));
    delete_event(
        &exec,
        Some(&base),
        &EventDeletion::occurrence(
            &base,
            Occurrence::starting(CalendarDateTime::utc(
                "2026-07-02T14:00:00".parse().unwrap(),
            )),
            stamp(),
        ),
    )
    .await
    .unwrap();

    let writes = exec.writes();
    assert_eq!(writes[0].method, DavMethod::Put);
    assert_eq!(writes[0].href, href().as_str());
    assert!(
        writes[0].body.contains("EXDATE:20260702T140000Z\r\n"),
        "the occurrence is excluded from the stored series: {}",
        writes[0].body
    );
    assert!(
        writes[0].body.contains("RRULE:FREQ=WEEKLY\r\n"),
        "and the rule the rest of the series runs on is untouched"
    );
    assert_eq!(
        writes[0].precondition,
        Precondition::IfMatch("\"v1\"".to_owned()),
        "it is an edit of the series, so it is guarded like one"
    );
}

#[tokio::test]
async fn removing_one_occurrence_without_the_stored_document_is_refused() {
    // The alternative is rebuilding the series from the lossy projection, which would
    // silently drop the VALARMs, the X- properties and the attendees on its way to
    // removing one occurrence.
    let exec = Replay::new(vec![wrote(204, None)]);
    let base = series(Some("\"v1\""));
    let deletion = EventDeletion::occurrence(
        &base,
        Occurrence::starting(CalendarDateTime::utc(
            "2026-07-02T14:00:00".parse().unwrap(),
        )),
        stamp(),
    );
    let err = delete_event(&exec, None, &deletion).await.unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(
        exec.writes().is_empty(),
        "and nothing reached the network on the way to failing"
    );
}
