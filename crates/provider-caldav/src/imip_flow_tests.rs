//! The end-to-end iTIP/iMIP flow: parse an inbound invite off the mail path → trust it
//! against its organizer → reconcile → RSVP → the outbox → a real store.
//!
//! It lives apart from `provider_tests` because it is one cohesive *scenario* rather than a
//! provider unit test: it is the only place the inbound parse, the trust decision and the
//! outbound answer are exercised as one flow.
//!
//! The answer goes through the **neutral RSVP verb** (`engine_provider::EventRsvp`), not
//! through a host-assembled document: the host states "accept, as this address", and the
//! adapter is what knows that CalDAV expresses it as a `PARTSTAT` rewrite in the stored
//! iCalendar plus a conditional `PUT` (`caldav.md`).

use core::time::Duration;

use engine_core::ids::AccountId;
use engine_provider::IgnoreConnectSteps;
use engine_store::{ManualClock, WorkerId};
use store_sqlite::SqliteStore;

use super::CalDavProvider;
use crate::test_support::{Replay, options};

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
        scheduling::{ScheduleAction, reconcile},
        version::ETag,
    };
    use engine_ical::parse_calendar_object;
    use engine_provider::{EventRsvp, RsvpResponse};
    use engine_sync::rsvp_calendar_event;

    use crate::{
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

    // (3) Drive the answer through the neutral RSVP verb into a real store. The host
    // never builds a document: it says "accept, as me@test.local", and the adapter
    // rewrites my PARTSTAT in the stored iCalendar and PUTs it back.
    // Discovery consumes PRINCIPAL and the scheduling `OPTIONS`; the PUT consumes the
    // write response. The probe answers `calendar-auto-schedule`, because this flow is
    // the auto-schedule one: the `PUT` *is* the whole RSVP only where the server turns
    // the changed PARTSTAT into the iTIP `REPLY` itself.
    let exec = std::sync::Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, calendar-access, calendar-auto-schedule")),
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

    let outcome = rsvp_calendar_event(
        &provider,
        &store,
        &account,
        WorkerId::new("t"),
        Duration::from_mins(5),
        "rsvp:meeting-7@test.local:accept",
        &stored,
        &EventRsvp::to(&stored, me, RsvpResponse::Accepted),
    )
    .await
    .expect("rsvp write");

    // (4) The op succeeded and recorded the server's new ETag.
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

#[tokio::test]
async fn caldav_refuses_the_two_controls_it_cannot_honour_rather_than_dropping_them() {
    // The RSVP goes out as a `PUT` that an RFC 6638 server turns into an iTIP REPLY on its
    // own. So "don't tell the organizer" is not something this transport can do, and
    // iCalendar has nowhere to carry a note. Silently ignoring either would leave the user
    // believing something that is not true — that the organizer got their message, or that
    // they did not get an email. Both must therefore be errors, and the capability is what
    // stops a host reaching them.
    use engine_core::{ids::CalendarId, version::ETag};
    use engine_ical::parse_calendar_object;
    use engine_provider::{EventRsvp, Provider, RsvpResponse};

    use crate::test_support::{Replay, ok, options};

    let exec = std::sync::Arc::new(Replay::new(vec![ok(PRINCIPAL), options(None)]));
    let provider = CalDavProvider::with_executor(
        Box::new(exec.clone()),
        "/.well-known/caldav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let mut stored = parse_calendar_object(
        STORED_INVITE,
        provider
            .event_href(&engine_core::ids::Uid::new("meeting-7@test.local").unwrap())
            .unwrap(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("parse stored invite");
    stored.revisions = engine_core::version::RevisionTokens::from_etag(ETag::new("\"rt-v1\""));

    // What the adapter advertises — and what a host reads before offering either control.
    let controls = provider
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("caldav can rsvp");
    assert!(!controls.comment);
    assert!(!controls.suppress_notification);

    let with_note = EventRsvp::to(&stored, "me@test.local", RsvpResponse::Accepted).comment("Hi");
    let err = provider
        .rsvp_event(&account, &stored, &with_note)
        .await
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);

    let quiet = EventRsvp::to(&stored, "me@test.local", RsvpResponse::Declined).quietly();
    let err = provider
        .rsvp_event(&account, &stored, &quiet)
        .await
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);

    // Neither refusal reached the network: a control we cannot honour is refused *before*
    // the write, not after a half-applied one.
    assert!(exec.writes().is_empty());
}

#[tokio::test]
async fn answering_an_invitation_you_are_not_on_is_refused() {
    // `set_my_partstat` has no ATTENDEE to rewrite, so the answer would otherwise be a PUT
    // of an unchanged document that the server reports as success — a button that does
    // nothing and says it worked.
    use engine_core::{ids::CalendarId, version::ETag};
    use engine_ical::parse_calendar_object;
    use engine_provider::{EventRsvp, Provider, RsvpResponse};

    use crate::test_support::{Replay, ok, options};

    let exec = std::sync::Arc::new(Replay::new(vec![ok(PRINCIPAL), options(None)]));
    let provider = CalDavProvider::with_executor(
        Box::new(exec.clone()),
        "/.well-known/caldav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let mut stored = parse_calendar_object(
        STORED_INVITE,
        provider
            .event_href(&engine_core::ids::Uid::new("meeting-7@test.local").unwrap())
            .unwrap(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("parse stored invite");
    stored.revisions = engine_core::version::RevisionTokens::from_etag(ETag::new("\"rt-v1\""));

    let err = provider
        .rsvp_event(
            &account,
            &stored,
            &EventRsvp::to(&stored, "stranger@example.com", RsvpResponse::Accepted),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
    assert!(exec.writes().is_empty());
}
