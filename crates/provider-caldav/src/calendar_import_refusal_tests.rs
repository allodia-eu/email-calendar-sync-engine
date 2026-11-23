//! What a refused import answers: the object's own refusal (`item_unavailable`), the calendar's
//! (a full store, a missing calendar, no permission: the target must change first), and a
//! throttle, which is neither. Each reads the one classifier a DAV write's answer has
//! (`CalDavError::write_refusal`). Several come from an independent review (R1, R3, R5).

use engine_core::{error::FailureClass, ids::EventId};
use engine_provider::{TargetRefusal, import::CalendarImporter};

use super::tests::{
    EVENT, NONE_HELD, PROPS, STALWART_INVALID, account, event, import_one, key, target,
};
use crate::test_support::{ok, status, wrote};

/// Stalwart's refusal of a request over its concurrency cap, as the send funnel hands it back
/// once it has given up waiting (`throttle.rs`).
const THROTTLED: &str = include_str!("../tests/fixtures/concurrency-limit-stalwart.json");

/// SabreDAV's refusal of a `PUT` into a calendar shared read-only (observed live).
const NEED_PRIVILEGES: &str = "<d:error xmlns:d=\"DAV:\"><d:need-privileges><d:resource>\
    <d:href>/calendars/alice@test.local/bob-readonly</d:href><d:privilege><d:bind/>\
    </d:privilege></d:resource></d:need-privileges></d:error>";

#[tokio::test]
async fn a_calendar_that_refuses_every_object_is_not_an_object_refusal() {
    let quota = "<d:error xmlns:d=\"DAV:\"><d:quota-not-exceeded/></d:error>";
    for (refusal, why) in [
        (
            status(403, NEED_PRIVILEGES),
            TargetRefusal::ReadOnlyContainer,
        ),
        // Stalwart and Nextcloud answer a read-only calendar with a plain `403` (observed).
        (status(403, ""), TargetRefusal::ReadOnlyContainer),
        // SabreDAV and Nextcloud answer a missing calendar `404`, Stalwart `409` (observed).
        (status(404, ""), TargetRefusal::NoSuchContainer),
        (status(409, ""), TargetRefusal::NoSuchContainer),
        (status(507, ""), TargetRefusal::StorageFull),
        (status(403, quota), TargetRefusal::StorageFull),
    ] {
        let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), refusal]).await;
        let refused = answer.unwrap_err();
        assert!(!refused.is_item_unavailable(), "{refused:?}");
        assert_eq!(refused.class(), FailureClass::InvalidState, "{refused:?}");
        assert!(!refused.requires_confirmation());
        assert_eq!(refused.target_refusal(), Some(why));
        assert!(!refused.detail().contains("bob-readonly"));
    }
}

#[tokio::test]
async fn a_throttled_put_or_lookup_is_a_wait_never_a_refusal_or_a_skipped_lookup() {
    let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), status(400, THROTTLED)]).await;
    let throttled = answer.unwrap_err();
    assert_eq!(throttled.class(), FailureClass::RateLimited);
    assert!(!throttled.is_item_unavailable());

    let (answer, replay) =
        import_one(vec![ok(PROPS), status(400, THROTTLED), wrote(201, None)]).await;
    assert_eq!(answer.unwrap_err().class(), FailureClass::RateLimited);
    assert!(
        replay.writes().is_empty(),
        "a lookup that was not answered is not a lookup that found nothing"
    );
}

#[tokio::test]
async fn a_name_taken_by_another_uid_is_a_conflict_not_a_link() {
    let other = EVENT.replace("capture-1@example.com", "someone-else@example.com");
    let (answer, _) = import_one(vec![
        ok(PROPS),
        ok(NONE_HELD),
        status(412, ""),
        status(200, &other),
    ])
    .await;
    assert_eq!(answer.unwrap_err().class(), FailureClass::Conflict);

    let (answer, _) = import_one(vec![
        ok(PROPS),
        ok(NONE_HELD),
        status(412, ""),
        status(503, ""),
    ])
    .await;
    assert_eq!(answer.unwrap_err().class(), FailureClass::Retryable);
}

#[tokio::test]
async fn a_holder_named_with_an_entity_is_the_href_it_stands_for() {
    let body = "<D:error xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:caldav\">\
        <A:no-uid-conflict><D:href>/dav/cal/alice%40test.local/work/a&amp;b.ics</D:href>\
        </A:no-uid-conflict></D:error>";
    let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), status(409, body)]).await;
    assert_eq!(
        key(&answer.expect("linked")),
        Some("/dav/cal/alice%40test.local/work/a&b.ics")
    );
}

#[tokio::test]
async fn the_size_checked_is_the_size_stored() {
    // Under the calendar's 524,288 bytes as the source has it, over once every party is
    // marked `SCHEDULE-AGENT=CLIENT`.
    let padding = 524_288 - EVENT.len() - 10;
    let near = EVENT.replace(
        "SUMMARY:Captured",
        &format!("SUMMARY:{}", "x".repeat(padding)),
    );
    assert!(near.len() < 524_288);
    let (provider, replay) = target(vec![ok(PROPS)]).await;

    let answers = provider.import_events(&account(), &[event(&near)]).await;

    assert!(answers[0].as_ref().unwrap_err().is_item_unavailable());
    assert!(replay.writes().is_empty());
}

#[tokio::test]
async fn a_task_holding_the_uid_is_not_the_event() {
    let task = "<D:multistatus xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:caldav\">\
        <D:response><D:href>/dav/cal/alice%40test.local/work/task.ics</D:href><D:propstat>\
        <D:prop><A:calendar-data>BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:capture-1@example.com\r\n\
        END:VTODO\r\nEND:VCALENDAR\r\n</A:calendar-data></D:prop>\
        <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let (answer, replay) = import_one(vec![ok(PROPS), ok(task), wrote(201, None)]).await;
    assert!(!answer.expect("stored").already_there);
    assert_eq!(replay.writes().len(), 1);
}

#[tokio::test]
async fn a_lookup_only_reads_the_uid() {
    // A held object the import could not itself store (it nests nothing) still holds the UID.
    let held = "<D:multistatus xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:caldav\">\
        <D:response><D:href>/dav/cal/alice%40test.local/work/held.ics</D:href><D:propstat>\
        <D:prop><A:calendar-data>BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:capture-1@example.com\r\n\
        ATTENDEE;CN=\"unbalanced:mailto:x@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n\
        </A:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>\
        </D:response></D:multistatus>";
    let (answer, replay) = import_one(vec![ok(PROPS), ok(held)]).await;
    assert_eq!(
        answer.expect("linked").key,
        Some(EventId::try_from("/dav/cal/alice%40test.local/work/held.ics").unwrap())
    );
    assert!(replay.writes().is_empty());
}

#[tokio::test]
async fn the_put_a_sync_create_reads_shares_the_rule() {
    // Stalwart refuses an invalid object `412` naming a precondition (STALWART-2): a refusal of
    // the object on every path, never the `If-None-Match` conflict a bare `412` is.
    let invalid = crate::error::CalDavError::status(412, STALWART_INVALID);
    assert_eq!(invalid.failure_class(), FailureClass::Permanent);
    assert_eq!(
        crate::error::CalDavError::status(412, "").failure_class(),
        FailureClass::Conflict
    );
}

/// A throttle or a refused credential reaches every event after it, so it ends the batch: the
/// rest are answered `not_sent` without a request (`providers.md` → "Import").
#[tokio::test]
async fn a_throttle_or_a_refused_credential_ends_the_batch() {
    for (ending, class) in [
        (status(400, THROTTLED), FailureClass::RateLimited),
        (status(401, ""), FailureClass::Authentication),
    ] {
        let (provider, replay) = target(vec![ok(PROPS), ok(NONE_HELD), ending]).await;
        let second = EVENT.replace("capture-1@", "capture-2@");
        let third = EVENT.replace("capture-1@", "capture-3@");

        let answers = provider
            .import_events(&account(), &[event(EVENT), event(&second), event(&third)])
            .await;

        assert_eq!(answers.len(), 3);
        for answer in &answers {
            assert_eq!(answer.as_ref().unwrap_err().class(), class);
        }
        assert!(
            answers[1]
                .as_ref()
                .unwrap_err()
                .detail()
                .starts_with("not sent")
        );
        assert_eq!(
            replay.seen().len(),
            5,
            "discovery, OPTIONS, limits, lookup, one PUT"
        );
    }
}
