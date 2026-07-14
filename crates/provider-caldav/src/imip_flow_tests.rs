//! The end-to-end iTIP/iMIP flow: parse an inbound invite off the mail path → trust it
//! against its organizer → reconcile → RSVP → the outbox → a real store.
//!
//! It lives apart from `provider_tests` because it is one cohesive *scenario* rather than a
//! provider unit test, and because it is the sole exercise of the whole-document write verb
//! — an RSVP is a finished document, not a property patch, which is why it does not go
//! through the neutral `patch_event` spine (`caldav.md`).

use core::time::Duration;

use engine_core::ids::AccountId;
use engine_provider::IgnoreConnectSteps;
use engine_store::{ManualClock, WorkerId};
use store_sqlite::SqliteStore;

use super::CalDavProvider;
use crate::test_support::Replay;

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");

/// A stored (no transit-only METHOD, RFC 4791 §4.1) copy of the invited event, as
/// my calendar holds it after a CalDAV auto-schedule server processed the REQUEST:
/// I am a needs-action attendee.
const STORED_INVITE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// The inbound iMIP REQUEST that delivered the invite (the same event, carrying a
/// `METHOD`), as parsed off the mail path.
const INVITE_REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nSEQUENCE:0\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

// One cohesive scenario (parse the invite → trust it against its organizer →
// accept → write my PARTSTAT back through the conditional-PUT outbox driver into a
// real store); splitting it would obscure the single inbound→outbound flow.
#[tokio::test]
async fn an_accepted_invite_rsvps_via_a_conditional_put_through_the_outbox() {
    use engine_core::{
        calendar::ParticipationStatus,
        ids::CalendarId,
        raw::RawIcal,
        scheduling::{ScheduleAction, reconcile},
        version::ETag,
    };
    use engine_provider::EventWrite;
    use engine_sync::put_calendar_document;

    use crate::{
        ical::parse_calendar_object,
        imip,
        test_support::{ok, wrote},
        transport::{DavMethod, Precondition},
    };

    // (1) Parse the inbound iMIP REQUEST off the mail path.
    let message = imip::parse(INVITE_REQUEST).expect("parse imip request");
    let uid = message.event.uid.clone();
    let me = "me@test.local";

    // (2) Trust + reconcile: the authenticated sender IS the organizer, and the
    // instance is unseen, so the decision is to schedule the event.
    assert_eq!(
        reconcile(&message, Some("boss@test.local"), None),
        ScheduleAction::ScheduleEvent
    );

    // (3) I accept: patch *my* PARTSTAT into my stored copy of the event (the RSVP
    // write primitive). Storage round-trips from raw plus this targeted patch.
    let patched = imip::set_my_partstat(
        &RawIcal::new(STORED_INVITE),
        me,
        &ParticipationStatus::Accepted,
    )
    .expect("rsvp patch");

    // (4) Drive the conditional PUT through the existing outbox driver into a real
    // store. Discovery consumes PRINCIPAL; the PUT consumes the write response.
    let exec = std::sync::Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        wrote(204, Some("\"rt-v2\"")),
    ]));
    let provider = CalDavProvider::with_executor(
        Box::new(exec.clone()),
        "/.well-known/caldav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let href = provider.event_href(&uid).expect("href");

    // My stored copy of the invite, as a sync would hand it back: the raw the RSVP was
    // patched from, and the revision it was read at (which becomes the write's guard).
    let mut stored = parse_calendar_object(
        STORED_INVITE,
        href.clone(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("parse stored invite");
    stored.revisions = engine_core::version::RevisionTokens::from_etag(ETag::new("\"rt-v1\""));

    // An RSVP is naturally a *finished document*, not a property patch — so it rides the
    // whole-document write verb, the one thing the neutral patch spine does not cover.
    let outcome = put_calendar_document(
        &provider,
        &store,
        &account,
        WorkerId::new("t"),
        Duration::from_mins(5),
        "rsvp:meeting-7@test.local:accept",
        &EventWrite::replacing(&stored, patched),
    )
    .await
    .expect("rsvp write");

    // (5) The op succeeded and recorded the server's new ETag.
    assert_eq!(outcome.event, href);
    assert_eq!(outcome.uid, uid);
    assert_eq!(outcome.revisions.etag, Some(ETag::new("\"rt-v2\"")));

    // The PUT was guarded by If-Match (optimistic concurrency, never a blind
    // overwrite), carried no transit-only METHOD, and its body sets my accepted
    // status while leaving the organizer's untouched.
    let writes = exec.writes();
    let put = writes
        .iter()
        .find(|w| w.method == DavMethod::Put)
        .expect("a PUT was issued");
    assert_eq!(
        put.precondition,
        Precondition::IfMatch("\"rt-v1\"".to_owned())
    );
    assert!(!put.body.contains("METHOD:"));
    let body = parse_calendar_object(
        &put.body,
        href.clone(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("the PUT body is valid iCalendar");
    let my_status = &body
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some(me))
        .unwrap()
        .participation_status;
    assert_eq!(my_status, &ParticipationStatus::Accepted);
    let boss = body
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("boss@test.local"))
        .unwrap();
    assert_eq!(boss.participation_status, ParticipationStatus::Accepted);
}

#[tokio::test]
async fn a_parsed_request_whose_organizer_mismatches_the_sender_is_rejected() {
    // The required security test (`calendar-semantics.md`), end to end on a *parsed*
    // message: the body's ORGANIZER is boss, but the authenticated sender is an
    // attacker — the bridge refuses it, so no write is ever planned.
    use engine_core::scheduling::{ImipUntrusted, ScheduleAction, reconcile};

    use crate::imip;

    let message = imip::parse(INVITE_REQUEST).expect("parse imip request");
    let action = reconcile(&message, Some("attacker@evil.example"), None);
    assert_eq!(
        action,
        ScheduleAction::Rejected(ImipUntrusted::SenderMismatch {
            expected: "organizer"
        })
    );
    assert!(
        !matches!(action, ScheduleAction::ScheduleEvent),
        "an untrusted invite is never scheduled"
    );
}
