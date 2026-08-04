//! Answering an invitation through the facade: the status the write moves, the address it
//! answers as, the guard it is refused on, and the control it refuses rather than drops.
//!
//! Split from `scenarios.rs` (which covers the ordinary write→reconcile path) so both stay
//! under the line limit; they share the stateful `CalendarServer` fake in the parent.

use engine_provider::RsvpResponse;
use engine_sync::SyncError;

use super::*;

#[tokio::test]
async fn answering_moves_our_own_status_and_leaves_the_store_holding_the_servers_copy() {
    // The neutral verb, end to end: intent in, the server's copy in the store, and the
    // answer readable without a further sync. If this ever stopped reconciling, a client
    // would render "you haven't answered" directly after the user answered — the exact
    // contradiction the product side of this feature had to fix.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;
    assert_eq!(
        status_of(&base, ALIAS_ADDRESS),
        ParticipationStatus::NeedsAction,
        "the seed must start unanswered, or this proves nothing"
    );

    let write = engine
        .rsvp_calendar_event(
            &server,
            &account(),
            "rsvp:evt-1:accept",
            &base,
            &EventRsvp::to(&base, ALIAS_ADDRESS, RsvpResponse::Accepted),
        )
        .await
        .unwrap();
    assert!(
        matches!(write.reconciled, Reconciled::Applied(_)),
        "got {:?}",
        write.reconciled
    );

    let stored = engine.events(&account()).await.unwrap().remove(0);
    assert_eq!(
        status_of(&stored, ALIAS_ADDRESS),
        ParticipationStatus::Accepted,
        "the answer must be readable from the store the moment the call returns"
    );
    assert_eq!(
        status_of(&stored, "organizer@test.local"),
        ParticipationStatus::NeedsAction,
        "an RSVP moves exactly one participant — ours — and leaves everyone else alone"
    );
    assert_eq!(stored.revisions.etag, Some(ETag::new("\"srv-2\"")));
}

#[tokio::test]
async fn the_answer_goes_out_as_the_address_the_invitation_matched() {
    // D5. The invitation reached `info@`; the account is `me@`. Answering as the account's
    // own address names an attendee this meeting does not have, and the server says so
    // rather than adding one — which would put the user on a meeting nobody invited them to.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    let refused = engine
        .rsvp_calendar_event(
            &server,
            &account(),
            "rsvp:evt-1:wrong-identity",
            &base,
            &EventRsvp::to(&base, SELF_ADDRESS, RsvpResponse::Accepted),
        )
        .await
        .expect_err("answering as an address the meeting has no ATTENDEE for must fail");
    assert!(
        format!("{refused}").contains("no ATTENDEE at that address"),
        "got {refused}"
    );

    // And the alias, which is what a caller that read the delivery headers would send, works.
    engine
        .rsvp_calendar_event(
            &server,
            &account(),
            "rsvp:evt-1:alias",
            &base,
            &EventRsvp::to(&base, ALIAS_ADDRESS, RsvpResponse::Tentative),
        )
        .await
        .expect("the matched address must be the one that answers");
    assert_eq!(
        status_of(&engine.events(&account()).await.unwrap()[0], ALIAS_ADDRESS),
        ParticipationStatus::Tentative
    );
}

#[tokio::test]
async fn answering_a_copy_the_organizer_has_since_changed_is_refused_not_applied() {
    // The guard is the whole reason the RSVP carries the revision it was read at. Without
    // it the answer lands on whatever the server now holds — a meeting that may have been
    // moved to another day — and the user has accepted something they never saw.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    // Someone else moves it. `base` is now a superseded copy.
    engine
        .patch_calendar_event(
            &server,
            &account(),
            "patch:evt-1:moved",
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap()).start(at(15)),
        )
        .await
        .unwrap();

    let stale = engine
        .rsvp_calendar_event(
            &server,
            &account(),
            "rsvp:evt-1:stale",
            &base,
            &EventRsvp::to(&base, ALIAS_ADDRESS, RsvpResponse::Accepted),
        )
        .await
        .expect_err("an answer guarded by a superseded revision must be refused");

    let ApiError::Sync(SyncError::Provider(err)) = &stale else {
        panic!("expected a provider error, got {stale:?}");
    };
    assert_eq!(
        err.class(),
        FailureClass::Conflict,
        "a stale guard is a Conflict — re-read and answer again, never blind-retry"
    );
    assert_eq!(
        status_of(&engine.events(&account()).await.unwrap()[0], ALIAS_ADDRESS),
        ParticipationStatus::NeedsAction,
        "and nothing was answered"
    );
}

#[tokio::test]
async fn a_control_this_transport_cannot_honour_fails_the_answer_rather_than_dropping_it() {
    // A note with nowhere to go, and a "don't tell them" a scheduling server will ignore.
    // Both must fail the write: a note that silently goes nowhere, or an organizer emailed
    // after the user asked for silence, is worse than a control never offered. A host that
    // read `Capabilities::calendar_rsvp` never gets here — which is what makes these two
    // errors a backstop rather than the interface.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    let controls = server
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .unwrap();
    assert!(!controls.comment && !controls.suppress_notification);

    for (idempotency, rsvp, expected) in [
        (
            "rsvp:evt-1:note",
            EventRsvp::to(&base, ALIAS_ADDRESS, RsvpResponse::Declined)
                .comment("Clashes with the offsite"),
            "nowhere to carry a note",
        ),
        (
            "rsvp:evt-1:quiet",
            EventRsvp::to(&base, ALIAS_ADDRESS, RsvpResponse::Declined).quietly(),
            "cannot be kept out of it",
        ),
    ] {
        let refused = engine
            .rsvp_calendar_event(&server, &account(), idempotency, &base, &rsvp)
            .await
            .expect_err("a control the transport cannot honour must fail the answer");
        assert!(
            format!("{refused}").contains(expected),
            "the error must name the control that was refused; got {refused}"
        );
    }

    assert_eq!(
        status_of(&engine.events(&account()).await.unwrap()[0], ALIAS_ADDRESS),
        ParticipationStatus::NeedsAction,
        "and neither attempt answered anything"
    );
}
