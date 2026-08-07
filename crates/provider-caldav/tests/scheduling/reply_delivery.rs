//! What an auto-scheduling server reports back about the reply it sent for us — the
//! `EventWriteReceipt::reply_delivery` half of an RSVP.
//!
//! The two-party fixture is the parent module ([`super`]); read its docs first.
//!
//! # Why this scenario asserts an absence, and how it earns the right to
//!
//! Stalwart writes **no** `ORGANIZER;SCHEDULE-STATUS` on the attendee's copy — verified here
//! and by direct DAV probing, on a delivered reply *and* with an unreachable organizer. So
//! the receipt is [`ReplyDelivery::NotReported`], and this test's headline assertion is that
//! something is *missing*.
//!
//! `AGENTS.md` is explicit that such a test has to prove the absence is the server's and not
//! ours, because "we never sent it" produces the identical observation. The proof is the
//! control below: the organizer's **separate** copy is polled until it shows
//! `PARTSTAT=ACCEPTED`, which can only happen if the reply was actually delivered. So the
//! pair reads *"the reply arrived **and** the server said nothing about it"* — which is a
//! server behaviour — rather than *"nothing happened"*, which would be a broken adapter.
//!
//! That distinction is the entire reason [`ReplyDelivery`] has three states instead of two:
//! a real deployment (Soverin, SabreDAV + `Schedule`) reports `5.2` on this exact property
//! and delivers nothing, so silence here cannot be read as either outcome.

use engine_core::{calendar::ParticipationStatus, ids::Uid};
use engine_provider::{EventRsvp, Provider, ReplyDelivery, RsvpResponse};

use super::{Parties, clean_up, invite, participant, poll_until};

const REPLY_DELIVERY_UID: &str = "caldav-schedule-reply-delivery@test.local";

/// Answering through the neutral verb returns a receipt whose `reply_delivery` reflects what
/// **this** server reported — nothing — while the reply itself demonstrably lands.
pub(crate) async fn an_rsvp_receipt_reports_what_the_server_said_about_delivery(parties: &Parties) {
    let uid = Uid::new(REPLY_DELIVERY_UID).unwrap();
    let mine = invite(
        parties,
        &uid,
        "Reply delivery reporting",
        "Europe/Amsterdam",
    )
    .await;

    let receipt = parties
        .attendee
        .rsvp_event(
            &parties.attendee_account,
            &mine,
            &EventRsvp::to(&mine, parties.attendee_address(), RsvpResponse::Accepted),
        )
        .await
        .expect("the neutral verb answers on an auto-scheduling server");

    // The control, first: the organizer's own resource — which we never wrote to — shows the
    // answer. Only a delivered iTIP REPLY puts it there, so the silence asserted next is the
    // server declining to report a delivery that provably happened.
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
        "the reply was delivered — so what follows is about reporting, not about failure"
    );

    assert_eq!(
        receipt.reply_delivery,
        ReplyDelivery::NotReported,
        "Stalwart writes no ORGANIZER;SCHEDULE-STATUS, so the receipt must claim nothing — \
         reporting a delivery it never observed is the bug this field exists to prevent"
    );
    assert!(
        !receipt.reply_delivery.failed(),
        "silence is not a failure either; only an explicit 3.x/5.x is actionable"
    );
    assert_eq!(
        receipt.reply_delivery.status(),
        None,
        "no token was reported, so there is none to put in a support log"
    );

    clean_up(parties, &uid).await;
}
