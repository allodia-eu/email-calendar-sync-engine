//! The live scenarios for issue #105: what an account needs when its calendar server does
//! **not** schedule for it.
//!
//! Both run against every real server the harness offers, and the pair is the point —
//! `scheduling_is_discovered_from_the_server` is the one assertion in this repo whose
//! expected value *differs between the two servers*, because it is reading a property of
//! the server rather than of the protocol. Stalwart advertises RFC 6638; the SabreDAV
//! fixture serves calendar access only. A fake cannot stand in for either: it answers
//! canned bytes without reading the request, so it could never tell us that a real
//! `OPTIONS` is answered at all, let alone what a real server puts in its `DAV:` header.
//!
//! [`storing_an_invitation_is_a_guarded_create`] is the other half. Putting the meeting on
//! the calendar has to be a create-if-absent, and only a server can tell us that
//! `If-None-Match: *` is honoured on a `PUT` — that a second one is refused rather than
//! silently overwriting the copy that is now there.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, Uid},
    raw::RawIcal,
};
use engine_provider::{EventDeletion, EventWrite, Provider};
use provider_caldav::CalDavProvider;

use super::{fetch, pre_clean};

/// The `UID` of the invitation this scenario stores and removes again.
const INVITATION_UID: &str = "caldav-imip-store@test.local";

/// Whether this server performs RFC 6638 scheduling, as **the provider discovered it** —
/// checked against what the server itself advertises.
///
/// `expected` is supplied by the caller because the two harness servers genuinely differ,
/// and that difference is the evidence: a capability that came out the same on both would
/// not be discovering anything.
pub(crate) fn scheduling_is_discovered_from_the_server(provider: &CalDavProvider, expected: bool) {
    let caps = provider.connection_info().capabilities;
    assert_eq!(
        caps.calendar_scheduling(),
        expected,
        "the discovered RFC 6638 capability must match what this server advertises in the \
         DAV: header of an OPTIONS response"
    );
    // Whatever the answer, the rest of the calendar capability is unchanged: both servers
    // read, write and can express an answer. Only the *delivery* promise differs — which is
    // exactly why "can I answer?" and "will anyone hear?" are two flags and not one.
    assert!(caps.calendars() && caps.calendar_writes());
    assert!(
        caps.calendar_rsvp().is_some(),
        "a PARTSTAT can be written on any CalDAV server; whether it is delivered is the \
         other question"
    );
}

/// Storing an invitation that arrived as mail: a **guarded create** whose second attempt
/// is refused rather than overwriting what is there.
///
/// This is the account shape issue #105 describes — mail on one transport, a calendar that
/// does no scheduling on another — so nothing puts the meeting on the calendar but the
/// host. It goes in through the whole-document verb because the invitation's own `VEVENT`
/// must survive intact: an `EventDraft` carries neither `ORGANIZER` nor `ATTENDEE`, so a
/// create through the neutral spine would store a plain appointment with nothing to answer
/// on afterwards.
///
/// What only a real server can tell you: that `If-None-Match: *` is honoured on a `PUT` of
/// a calendar object resource, and that the refusal is a `412` the adapter classes
/// `Conflict`. That matters because the concurrent writer this guards against is usually
/// the **server itself** — an auto-scheduling one deposits its own copy the moment the
/// organizer writes, and an unconditional `PUT` would erase it along with whatever the
/// server had already recorded about delivery.
pub(crate) async fn storing_an_invitation_is_a_guarded_create(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    let uid = Uid::new(INVITATION_UID).unwrap();
    pre_clean(provider, account, &uid).await;
    let href = provider.event_href(&uid).expect("mint event href");

    // The invitation as it arrived, minus the transit-only METHOD (RFC 4791 §4.1 forbids
    // one in a stored resource). Its organizer, attendee and SEQUENCE are the point: they
    // are what an RSVP later needs, and what no `EventDraft` can carry.
    let invitation = RawIcal::new(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Organizer//EN\r\nBEGIN:VEVENT\r\n\
         UID:caldav-imip-store@test.local\r\nDTSTAMP:20260501T080000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260604T090000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260604T093000\r\nSUMMARY:Invitation from mail\r\n\
         ORGANIZER;CN=Boss:mailto:boss@test.local\r\n\
         ATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\n\
         ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\n\
         SEQUENCE:3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    );

    let receipt = provider
        .put_event(
            account,
            &EventWrite::creating(href.clone(), uid.clone(), invitation.clone()),
        )
        .await
        .expect("a create onto a free href lands");
    assert_eq!(receipt.uid, uid);

    // The server has it, and it is answerable: the ATTENDEE line an RSVP needs survived,
    // still at NEEDS-ACTION.
    let stored = fetch(provider, account, INVITATION_UID)
        .await
        .expect("the invitation is on the calendar");
    assert_eq!(stored.title, "Invitation from mail");
    let me = stored
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("me@test.local"))
        .expect("my ATTENDEE line survived the store, so there is something to answer on");
    assert_eq!(
        me.participation_status,
        engine_core::calendar::ParticipationStatus::NeedsAction
    );
    assert!(
        stored
            .participants
            .iter()
            .any(|p| p.email.as_deref() == Some("boss@test.local")),
        "the organizer survived too — an EventDraft could not have carried either"
    );

    // ---- The guard: a second create onto the now-occupied href is refused. ----
    let error = provider
        .put_event(
            account,
            &EventWrite::creating(href.clone(), uid.clone(), invitation.clone()),
        )
        .await
        .expect_err("If-None-Match: * must refuse a resource that already exists");
    assert_eq!(
        error.class(),
        FailureClass::Conflict,
        "an occupied href is a Conflict — re-read and decide, never a blind retry"
    );
    assert!(!error.is_retryable());

    // …and the refusal changed nothing: the first copy is still there, untouched.
    let survivor = fetch(provider, account, INVITATION_UID)
        .await
        .expect("the refused create left the stored copy alone");
    assert_eq!(survivor.title, "Invitation from mail");

    // For contrast, the same document written *unconditionally* does land — which is what
    // the guarded create is protecting against, and why it cannot be the default.
    provider
        .put_event(
            account,
            &EventWrite::unconditional(href, uid.clone(), invitation),
        )
        .await
        .expect("an unconditional write overwrites whatever is there");

    let final_copy = fetch(provider, account, INVITATION_UID)
        .await
        .expect("still present");
    provider
        .delete_event(account, None, &EventDeletion::of(&final_copy))
        .await
        .expect("clean up");
}
