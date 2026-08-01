//! What each live scheduling scenario asserts.
//!
//! The two-party fixture these run on — who the parties are, how an invitation is created,
//! waited for, and cleaned up, and why both parties are scratch accounts — is the parent
//! module ([`super`]). Read its docs first; they carry the observed server behaviour.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    error::FailureClass,
    ids::Uid,
    scheduling::{ScheduleMethod, addresses_match},
};
use engine_provider::{EventRsvp, EventWrite, Provider, RsvpResponse};
use provider_caldav::imip;

use super::{
    Parties, clean_up, invite, normalized, organizer_inbox_hrefs, participant, poll_until,
    scheduling_inbox_hrefs,
};
use crate::common;

// ---------------------------------------------------------------------------
// 1. Delivery: the server puts the invitation on the attendee's calendar.
// ---------------------------------------------------------------------------

const DELIVERED_UID: &str = "caldav-schedule-delivered@test.local";

/// An invitation the organizer stores arrives on the **attendee's** calendar, normalized,
/// with the attendee still owing a reply.
///
/// This is the `ServerAutoSchedule` half of the capability split: the attendee's client sent
/// nothing and parsed no iMIP, yet the meeting is on their calendar. It also pins which
/// participant is which — a reader that mixed up `ORGANIZER` and `ATTENDEE` would offer the
/// organizer an RSVP to their own meeting.
pub(crate) async fn an_invitation_is_delivered_to_the_attendee(parties: &Parties) {
    let uid = Uid::new(DELIVERED_UID).unwrap();
    let mine = invite(
        parties,
        &uid,
        "Live scheduling invitation",
        "Europe/Amsterdam",
    )
    .await;

    assert_eq!(mine.title, "Live scheduling invitation");
    assert_eq!(
        mine.uid.as_str(),
        DELIVERED_UID,
        "the cross-system UID survives delivery — it is what reconciles the two copies"
    );

    let me = participant(&mine, parties.attendee_address());
    assert_eq!(
        me.participation_status,
        ParticipationStatus::NeedsAction,
        "the delivered copy owes a reply; the server does not answer for the user"
    );
    assert!(
        me.expect_reply,
        "RSVP=TRUE survived onto the attendee's copy"
    );

    let organizer = participant(&mine, parties.organizer_address());
    assert!(
        organizer.has_role(&engine_core::calendar::ParticipantRole::Owner),
        "the ORGANIZER reads back as the owning participant, not as a plain attendee"
    );

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 2. The Windows time zone, as a real server hands it back.
// ---------------------------------------------------------------------------

const WINDOWS_ZONE_UID: &str = "caldav-schedule-winzone@test.local";

/// An invitation carrying a **Windows** zone name resolves to a real IANA zone on the
/// attendee's copy.
///
/// `DTSTART;TZID=W. Europe Standard Time:…` is what Outlook sends, and it reaches a CalDAV
/// account whenever an Exchange organizer invites one. Two things only a real server can
/// settle: Stalwart **accepts** such a `TZID` with no accompanying `VTIMEZONE`, and it hands
/// the parameter back **DQUOTE-quoted** (the value contains spaces and dots — RFC 5545 §3.1).
/// So the parser must strip the quotes *and* map the name through CLDR; getting either wrong
/// leaves the event zoned to a name no tzdb resolves — no instant, so it is unplaceable on a
/// grid and invisible to a conflict check. Verified to bite: restore the old
/// `TimeZoneId::iana(tzid)` call in `engine-ical`'s value parser and this reads back
/// `"W. Europe Standard Time"`.
pub(crate) async fn an_invitations_windows_time_zone_resolves_to_iana(parties: &Parties) {
    let uid = Uid::new(WINDOWS_ZONE_UID).unwrap();
    let mine = invite(
        parties,
        &uid,
        "Windows zone invitation",
        "W. Europe Standard Time",
    )
    .await;

    let raw = common::server_ical(&mine);
    assert!(
        raw.as_str().contains(r#"TZID="W. Europe Standard Time""#),
        "Stalwart quotes a TZID containing spaces; got {}",
        raw.as_str()
    );

    let zone = mine.start.zone().expect("a zoned start");
    assert!(
        zone.is_iana(),
        "a Windows zone name must resolve to IANA, not be stored as a fake IANA id: {zone:?}"
    );
    assert_eq!(zone.as_str(), "Europe/Berlin");
    assert!(
        engine_recurrence::resolve_instant(&mine.start).is_ok(),
        "the resolved zone must yield a real instant, or the meeting cannot be placed"
    );

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 3. The scheduling inbox carries a parseable iTIP REQUEST.
// ---------------------------------------------------------------------------

const INBOX_UID: &str = "caldav-schedule-inbox@test.local";

/// The `METHOD:REQUEST` the server deposits in the attendee's RFC 6638 scheduling inbox
/// parses through the engine's one iCalendar parser.
///
/// The inbox document is *not* the stored copy: it is a transit-form iTIP message, folded and
/// re-serialized by the server, carrying the `METHOD` a stored resource must never have
/// (RFC 4791 §4.1). Parsing a real one proves the scheduling parser is fed by something other
/// than our own serializer.
pub(crate) async fn the_scheduling_inbox_carries_a_parseable_itip_request(parties: &Parties) {
    let uid = Uid::new(INBOX_UID).unwrap();
    invite(parties, &uid, "Inbox request", "Europe/Amsterdam").await;

    let hrefs = scheduling_inbox_hrefs(parties, uid.as_str());
    assert!(
        !hrefs.is_empty(),
        "the auto-schedule server deposits an iTIP message for the attendee"
    );
    let body = parties
        .harness
        .dav_get_as(parties.attendee_auth.auth(), &hrefs[0])
        .expect("GET the scheduling-inbox message");
    let text = String::from_utf8_lossy(&body.body).into_owned();

    let message = engine_ical::parse_scheduling_message(&text).expect("a scheduling message");
    assert_eq!(message.method, ScheduleMethod::Request);
    assert_eq!(message.event.uid.as_str(), uid.as_str());
    assert_eq!(
        message.organizer().map(normalized),
        Some(parties.organizer_address().to_owned()),
        "the ORGANIZER identity a trust decision would be made against"
    );
    assert!(
        message
            .event
            .participants
            .iter()
            .filter_map(|p| p.email.as_deref())
            .any(|email| addresses_match(email, parties.attendee_address())),
        "an ATTENDEE matching the receiving account — what makes this invitation *mine*"
    );

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 4. The headline: an RSVP reaches the organizer with no delivery step.
// ---------------------------------------------------------------------------

const RSVP_UID: &str = "caldav-schedule-rsvp@test.local";

/// The attendee accepts by patching *their own* `PARTSTAT` and `PUT`ting the resource
/// back — and the **organizer's separate copy** shows the acceptance.
///
/// This is the claim `calendar-semantics.md` stakes the whole RSVP design on: storage and
/// delivery are one operation on an auto-schedule server, so the engine ships no iTIP
/// `REPLY` assembler and no SMTP send for this path. Nothing offline can test it — the
/// assertion is about what the server did to *another account's* resource after reading
/// bytes from ours.
///
/// Verified to fail for the right reason: with the patcher stubbed to store the document
/// unchanged, the `PUT` still succeeds, and this times out waiting for a `REPLY` the server
/// had no reason to send. What it does **not** police is the `PARTSTAT` *spelling* —
/// Stalwart accepts a lowercase `accepted`, because RFC 5545 §3.1 parameter values are
/// case-insensitive. The uppercasing in `set_my_partstat` is canonical-form hygiene, not a
/// compatibility requirement this server can prove.
pub(crate) async fn an_rsvp_reaches_the_organizer(parties: &Parties) {
    let uid = Uid::new(RSVP_UID).unwrap();
    let mine = invite(parties, &uid, "RSVP round trip", "Europe/Amsterdam").await;

    // The RSVP write primitive: my PARTSTAT into the stored document, every other byte
    // untouched, guarded by the revision the read reported.
    let accepted = imip::set_my_partstat(
        &common::server_ical(&mine),
        parties.attendee_address(),
        &ParticipationStatus::Accepted,
    )
    .expect("patch my PARTSTAT");
    parties
        .attendee
        .put_event(
            &parties.attendee_account,
            &EventWrite::replacing(&mine, accepted),
        )
        .await
        .expect("store the RSVP");

    // The organizer's own copy — a different resource, in a different account, that we
    // never wrote to.
    let theirs = poll_until(
        &parties.organizer,
        &parties.organizer_account,
        &uid,
        "the iTIP REPLY to reach the organizer's copy",
        |event| {
            event
                .participants
                .iter()
                .any(|p| p.participation_status == ParticipationStatus::Accepted)
        },
    )
    .await;
    assert_eq!(
        participant(&theirs, parties.attendee_address()).participation_status,
        ParticipationStatus::Accepted,
        "the server derived the REPLY from the stored PARTSTAT and applied it"
    );

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 4b. The same round trip, through the verb a host actually calls.
// ---------------------------------------------------------------------------

const NEUTRAL_RSVP_UID: &str = "caldav-schedule-rsvp-verb@test.local";

/// The attendee answers through `Provider::rsvp_event` — the neutral verb — and the
/// **organizer's separate copy** shows it.
///
/// [`an_rsvp_reaches_the_organizer`] above drives the *primitive* (`set_my_partstat` plus a
/// whole-document `PUT`). This drives what a host actually calls, which is a different code
/// path: the adapter resolves the answering address against the stored `ATTENDEE` lines,
/// patches the document itself, and guards the `PUT` on the revision the read reported. A
/// green primitive says nothing about that assembly — which is why this is a second
/// scenario rather than a rewrite of the first.
///
/// It also pins the two refusals against a **real** server rather than a fake that was told
/// to refuse: CalDAV advertises neither a note nor a notify toggle, and asking for either
/// must fail *before* anything is written. That the invitation is still unanswered
/// afterwards is the half that matters — a refusal that had already answered is worse than
/// no refusal at all.
pub(crate) async fn an_rsvp_through_the_neutral_verb_reaches_the_organizer(parties: &Parties) {
    let uid = Uid::new(NEUTRAL_RSVP_UID).unwrap();
    let mine = invite(
        parties,
        &uid,
        "RSVP via the neutral verb",
        "Europe/Amsterdam",
    )
    .await;

    let controls = parties
        .attendee
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("CalDAV advertises that it can answer an invitation");
    assert!(
        !controls.comment && !controls.suppress_notification,
        "an RFC 6638 server replies the moment the PARTSTAT changes: nowhere to put a note, \
         and no way to keep the organizer out of it"
    );

    // Both refusals, against the real server, before the answer that works.
    for (label, rsvp) in [
        (
            "a note",
            EventRsvp::to(&mine, parties.attendee_address(), RsvpResponse::Accepted)
                .comment("See you there"),
        ),
        (
            "silence",
            EventRsvp::to(&mine, parties.attendee_address(), RsvpResponse::Accepted).quietly(),
        ),
    ] {
        let refused = parties
            .attendee
            .rsvp_event(&parties.attendee_account, &mine, &rsvp)
            .await
            .expect_err("asking for a control CalDAV cannot honour must fail");
        assert_eq!(
            refused.class(),
            FailureClass::InvalidState,
            "{label}: a control this transport cannot honour is a caller error, not a retry"
        );
    }
    let still_unanswered =
        common::fetch(&parties.attendee, &parties.attendee_account, uid.as_str())
            .await
            .expect("the invitation is still on the attendee's calendar");
    assert_eq!(
        participant(&still_unanswered, parties.attendee_address()).participation_status,
        ParticipationStatus::NeedsAction,
        "a refused control must leave the invitation unanswered"
    );

    // The answer a host sends: the matched address, the response, nothing else.
    parties
        .attendee
        .rsvp_event(
            &parties.attendee_account,
            &mine,
            &EventRsvp::to(&mine, parties.attendee_address(), RsvpResponse::Declined),
        )
        .await
        .expect("the neutral verb must answer on a server-scheduled transport");

    let theirs = poll_until(
        &parties.organizer,
        &parties.organizer_account,
        &uid,
        "the iTIP REPLY from the neutral verb to reach the organizer's copy",
        |event| {
            event
                .participants
                .iter()
                .any(|p| p.participation_status == ParticipationStatus::Declined)
        },
    )
    .await;
    assert_eq!(
        participant(&theirs, parties.attendee_address()).participation_status,
        ParticipationStatus::Declined,
        "the server derived the REPLY from what the neutral verb stored"
    );

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 5. An organizer's cancellation reaches the attendee's copy.
// ---------------------------------------------------------------------------

const CANCEL_UID: &str = "caldav-schedule-cancel@test.local";

/// When the organizer deletes the meeting, the attendee's copy comes back **cancelled**
/// rather than vanishing.
///
/// Worth pinning because it is not what a client might assume: the server does not remove
/// the attendee's resource, it applies the iTIP `CANCEL` as `STATUS:CANCELLED` — which the
/// projection reads as a tombstone. A host that only listened for deletions would show a
/// cancelled meeting as still on. It also means an attendee's own copy outlives the
/// organizer's, so cleanup must delete both.
pub(crate) async fn an_organizer_cancel_marks_the_attendees_copy_cancelled(parties: &Parties) {
    let uid = Uid::new(CANCEL_UID).unwrap();
    let mine = invite(parties, &uid, "Cancelled meeting", "Europe/Amsterdam").await;
    assert!(
        !mine.is_cancelled(),
        "the invitation starts out live, or the cancel below proves nothing"
    );

    common::pre_clean(&parties.organizer, &parties.organizer_account, &uid).await;

    let cancelled = poll_until(
        &parties.attendee,
        &parties.attendee_account,
        &uid,
        "the iTIP CANCEL to reach the attendee's copy",
        Event::is_cancelled,
    )
    .await;
    assert!(cancelled.is_cancelled());

    clean_up(parties, &uid).await;
}

// ---------------------------------------------------------------------------
// 6. Cleanup itself: a scenario leaves nothing behind on either side.
// ---------------------------------------------------------------------------

const RESIDUE_UID: &str = "caldav-schedule-residue@test.local";

/// A full round trip — invitation out, answer back — leaves **nothing** on the server:
/// neither calendar holds the event, and neither party's scheduling inbox holds a message
/// for it.
///
/// This asserts the *fixture*, not the adapter, and it exists because the fixture was
/// leaking. `clean_up` cleared the attendee's scheduling inbox and not the organizer's — but
/// scheduling runs both ways: the attendee gets a `REQUEST`, and every answer deposits a
/// `METHOD:REPLY` in the **organizer's** inbox. The suite's two RSVP scenarios therefore left
/// bob two replies richer every run (measured 0 → 2 → 4 across consecutive runs), unbounded
/// on a long-lived harness. CI never noticed because CI always starts from `down -v`.
///
/// It has to answer the invitation, not merely receive one: no RSVP, no `REPLY`, and the leak
/// this guards is invisible. Drop the organizer from the inbox sweep in [`clean_up`] and the
/// last assertion fails — verified, not assumed.
///
/// The assertion is about **final state** rather than a call sequence on purpose, because the
/// mechanism the tracking issue (#93) predicted turned out not to exist. Its guess was a
/// calendar-copy race, an organizer `DELETE` scheduling a `CANCEL` that re-creates the
/// attendee's already-deleted copy; probed in isolation on both v0.16.11 and v0.16.15, the
/// server simply drops a `CANCEL` for a resource the attendee has already removed. Asserting
/// state keeps this honest whichever way a future Stalwart decides to leak.
pub(crate) async fn cleanup_leaves_no_residue(parties: &Parties) {
    let uid = Uid::new(RESIDUE_UID).unwrap();
    let mine = invite(parties, &uid, "Residue check", "Europe/Amsterdam").await;

    parties
        .attendee
        .rsvp_event(
            &parties.attendee_account,
            &mine,
            &EventRsvp::to(&mine, parties.attendee_address(), RsvpResponse::Accepted),
        )
        .await
        .expect("answer the invitation, so the organizer is sent a REPLY to leave behind");
    // The REPLY has been processed once the organizer's own copy shows the answer — which is
    // also when its inbox message exists to be cleared.
    poll_until(
        &parties.organizer,
        &parties.organizer_account,
        &uid,
        "the REPLY to reach the organizer, so cleanup has something to clean",
        |event| {
            event
                .participants
                .iter()
                .any(|p| p.participation_status == ParticipationStatus::Accepted)
        },
    )
    .await;

    clean_up(parties, &uid).await;

    assert!(
        common::fetch(&parties.organizer, &parties.organizer_account, uid.as_str())
            .await
            .is_none(),
        "the organizer's copy survived cleanup"
    );
    assert!(
        common::fetch(&parties.attendee, &parties.attendee_account, uid.as_str())
            .await
            .is_none(),
        "the attendee's copy survived cleanup"
    );
    for (whose, left) in [
        ("attendee", scheduling_inbox_hrefs(parties, uid.as_str())),
        ("organizer", organizer_inbox_hrefs(parties, uid.as_str())),
    ] {
        assert!(
            left.is_empty(),
            "the {whose}'s scheduling inbox still holds {} iTIP message(s) for this event \
             ({left:?}); RFC 6638 §3.2 makes clearing a processed message the client's job, \
             and an uncleared one accumulates on every run",
            left.len()
        );
    }
}
