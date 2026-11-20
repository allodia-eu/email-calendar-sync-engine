//! The adapter refuses a meeting edit no CalDAV server can carry before it writes anything, and
//! as `InvalidState`, the class every calendar adapter gives it.

use engine_core::{
    calendar::{ConferenceKey, ConferenceService},
    error::FailureClass,
    ids::{AccountId, EventId},
    raw::RawIcal,
};
use engine_provider::{
    CalendarWrites, ConferenceEdit, EventEdit, EventPatch, IgnoreConnectSteps, NewConference,
    Occurrence, PatchTarget,
};

use super::*;
use crate::test_support::{Replay, ok, options};

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");
const SERIES: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:evt-1@test.local\r\n\
                      DTSTAMP:20260601T000000Z\r\nDTSTART:20260625T140000Z\r\n\
                      DTEND:20260625T150000Z\r\nRRULE:FREQ=WEEKLY\r\n\
                      SUMMARY:Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

async fn refused(target: PatchTarget, conference: ConferenceEdit) {
    let replay = std::sync::Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, calendar-access")),
    ]));
    let provider = CalDavProvider::with_executor(
        Box::new(std::sync::Arc::clone(&replay)),
        "/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let base = engine_ical::parse_calendar_object(
        SERIES,
        EventId::try_from("/dav/cal/alice%40test.local/work/evt-1.ics").unwrap(),
        engine_core::ids::CalendarId::try_from("/dav/cal/alice%40test.local/work/").unwrap(),
    )
    .unwrap();
    assert_eq!(base.raw_ical, Some(RawIcal::new(SERIES)));
    let edit = EventEdit::new(
        &base,
        target,
        EventPatch::new("2026-06-20T00:00:00Z".parse().unwrap()).conference(conference),
    );
    let error = provider
        .patch_event(&AccountId::try_from("a").unwrap(), &base, &edit)
        .await
        .unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
    assert!(replay.writes().is_empty());
}

#[tokio::test]
async fn a_patch_asking_the_server_for_a_meeting_is_refused_before_anything_is_written() {
    refused(
        PatchTarget::Series,
        ConferenceEdit::Set {
            replacing: None,
            new: NewConference::Native(ConferenceService::Teams),
        },
    )
    .await;
}

#[tokio::test]
async fn a_meeting_on_one_occurrence_is_refused_before_anything_is_written() {
    refused(
        PatchTarget::Instance(Occurrence::starting(
            engine_core::time::CalendarDateTime::utc("2026-07-02T14:00:00".parse().unwrap()),
        )),
        ConferenceEdit::Remove(ConferenceKey::new("https://meet.example.org/a")),
    )
    .await;
}
