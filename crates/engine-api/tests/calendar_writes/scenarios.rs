//! The read-your-writes scenarios (issue #65), driven against the stateful fake server in
//! the parent binary: a patch leaves the store holding the **server's** copy, a host can
//! edit the same event twice by re-reading it in between, a delete tombstones the local
//! row, a create lands under the id the server assigned, the RSVP document write reconciles
//! like any other, and a write whose reconcile could not run is still a write.

use engine_provider::RsvpResponse;
use engine_sync::SyncError;

use super::*;

/// The participation status the event records for `address` — the one thing an RSVP is
/// supposed to move, read the way a client reads it.
fn status_of(event: &Event, address: &str) -> ParticipationStatus {
    event
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some(address))
        .unwrap_or_else(|| panic!("no participant at {address}"))
        .participation_status
        .clone()
}

#[tokio::test]
async fn a_patch_leaves_the_store_holding_the_servers_copy_not_ours() {
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;
    assert_eq!(base.revisions.etag, Some(ETag::new("\"srv-1\"")));

    let write = engine
        .patch_calendar_event(
            &server,
            &account(),
            "patch:evt-1:rev1",
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap())
                .summary("Standup (moved)")
                .start(at(11)),
        )
        .await
        .unwrap();
    assert!(
        matches!(write.reconciled, Reconciled::Applied(_)),
        "got {:?}",
        write.reconciled
    );

    // The store no longer holds the pre-write event: it holds what the SERVER stored —
    // the server's re-serialization and the server's revision, neither of which we sent.
    let stored = engine.events(&account()).await.unwrap().remove(0);
    assert_eq!(stored.title, "Standup (moved)");
    assert_eq!(stored.revisions.etag, Some(ETag::new("\"srv-2\"")));
    assert!(
        stored
            .raw_ical
            .as_ref()
            .unwrap()
            .as_str()
            .contains("X-SERVER-SERIALIZED:srv-2"),
        "the stored document must be the server's, so a property the server dropped is \
         visible rather than masked by our own copy"
    );
    assert_eq!(
        write.write.revisions.etag,
        Some(ETag::new("\"srv-2\"")),
        "the receipt still reports the revision, for a host chaining writes off it"
    );

    // And the grid moved with it: one occurrence, at the new time.
    let occurrences = engine
        .occurrences_in(&account(), march_first())
        .await
        .unwrap();
    assert_eq!(occurrences.len(), 1, "the moved event must not ghost");
}

#[tokio::test]
async fn a_host_can_edit_the_same_event_twice_reading_it_back_in_between() {
    // The footgun the issue is named for. Without the post-write reconcile the store still
    // holds the pre-write revision, so this second edit — built from the store, exactly as
    // a host builds one — guards on a superseded ETag and the server refuses it with a
    // `412` on a write that should have succeeded.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    engine
        .patch_calendar_event(
            &server,
            &account(),
            "patch:evt-1:rev1",
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap()).summary("First edit"),
        )
        .await
        .unwrap();

    let reread = engine.events(&account()).await.unwrap().remove(0);
    let second = engine
        .patch_calendar_event(
            &server,
            &account(),
            "patch:evt-1:rev2",
            &reread,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:05:00Z".parse().unwrap()).summary("Second edit"),
        )
        .await
        .expect("the second edit must not be refused on a stale guard");

    assert!(matches!(second.reconciled, Reconciled::Applied(_)));
    assert_eq!(
        engine.events(&account()).await.unwrap()[0].title,
        "Second edit"
    );
}

#[tokio::test]
async fn a_delete_tombstones_the_local_row_instead_of_waiting_for_the_next_sync() {
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    let deleted = engine
        .delete_calendar_event(
            &server,
            &account(),
            "delete:evt-1",
            &EventDeletion::of(&base),
        )
        .await
        .unwrap();
    assert!(matches!(deleted.reconciled, Reconciled::Applied(_)));

    assert!(
        engine.events(&account()).await.unwrap().is_empty(),
        "the deleted event must be gone from the store, not linger until the next sync"
    );
    assert!(
        engine
            .occurrences_in(&account(), march_first())
            .await
            .unwrap()
            .is_empty(),
        "and its occurrence rows must go with it"
    );
}

#[tokio::test]
async fn a_create_stores_the_event_the_server_assigned() {
    let server = CalendarServer::holding(seeded_event());
    let (engine, _) = synced(&server).await;

    let write = engine
        .create_calendar_event(
            &server,
            &account(),
            "create:evt-2",
            &EventDraft::new(
                CalendarId::try_from("work").unwrap(),
                Uid::new("evt-2@test.local").unwrap(),
                "Retro",
                at(14),
                at(15),
                "2026-07-14T10:00:00Z".parse().unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(write.reconciled, Reconciled::Applied(_)));

    // The id a server-assigning transport reveals only on the receipt — and the store now
    // holds the event under it, without a further sync.
    let stored = engine.events(&account()).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(
        stored.iter().any(|e| e.id == write.write.event),
        "the created event must be in the store under the id the server assigned"
    );
}

#[tokio::test]
async fn a_write_whose_reconcile_cannot_run_is_still_a_write() {
    // The reconcile claims the event scope, so a sync already holding it makes the delta
    // impossible. That must NOT be reported as a failed write — the server has the change,
    // and a host that re-issued it would write twice. It is reported as `Busy`, with the
    // store honestly still holding the pre-write copy until something re-reads it.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    // Hold the event scope for the duration of the write, as a concurrent sync would.
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (started, wait) = tokio::sync::oneshot::channel::<()>();
    let blocker = BlockingSync {
        inner: server.clone(),
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(held)),
    };
    let engine = Arc::new(engine);
    let holder = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.reconcile_calendar_events(&blocker, &account()).await })
    };
    wait.await.unwrap();

    let write = engine
        .patch_calendar_event(
            &server,
            &account(),
            "patch:evt-1:rev1",
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap()).summary("Renamed"),
        )
        .await
        .expect("the write landed; only the local re-read could not run");
    assert!(
        matches!(write.reconciled, Reconciled::Busy),
        "got {:?}",
        write.reconciled
    );
    assert_eq!(
        write.write.revisions.etag,
        Some(ETag::new("\"srv-2\"")),
        "and the receipt carries the revision the store does not have yet — keep it"
    );

    release.send(()).unwrap();
    holder.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_rsvp_document_write_reconciles_like_any_other() {
    // `put_calendar_document` is the escape hatch for an operation that is naturally a
    // finished document rather than a property patch (the iMIP RSVP primitive). It leaves
    // the store just as stale as a patch would, so it reconciles on the same terms — and
    // the store ends up with the SERVER's document, not the one we handed it.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;

    let write = engine
        .put_calendar_document(
            &server,
            &account(),
            "rsvp:evt-1:accept",
            &EventWrite::replacing(
                &base,
                RawIcal::new("BEGIN:VCALENDAR\r\nX-CLIENT-WROTE-THIS:1\r\nEND:VCALENDAR"),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(write.reconciled, Reconciled::Applied(_)));

    let stored = engine.events(&account()).await.unwrap().remove(0);
    assert_eq!(stored.revisions.etag, Some(ETag::new("\"srv-2\"")));
    assert!(
        stored
            .raw_ical
            .as_ref()
            .unwrap()
            .as_str()
            .contains("X-SERVER-SERIALIZED:srv-2"),
        "the store holds what the server stored, not the bytes the RSVP write sent"
    );
}

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

#[tokio::test]
async fn a_reconcile_that_fails_reports_why_and_still_keeps_the_write() {
    // Not `Busy` — the delta itself failed (here the provider's event fetch is down; a
    // dropped connection right after the write does the same). The write is already
    // committed on the server, so this must not surface as an error: it comes back as
    // `Failed`, with the store honestly still holding the pre-write copy.
    let server = CalendarServer::holding(seeded_event());
    let (engine, base) = synced(&server).await;
    let broken = UnreadableEvents(server.clone());

    let write = engine
        .patch_calendar_event(
            &broken,
            &account(),
            "patch:evt-1:rev1",
            &base,
            PatchTarget::Series,
            EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap()).summary("Renamed"),
        )
        .await
        .expect("the write landed; only the re-read failed");

    let Reconciled::Failed(err) = &write.reconciled else {
        panic!("expected Failed, got {:?}", write.reconciled);
    };
    // The error is carried whole, so a host can still classify it rather than grep a string.
    let ApiError::Sync(SyncError::Provider(provider_err)) = err.as_ref() else {
        panic!("expected a provider error, got {err:?}");
    };
    assert_eq!(provider_err.class(), FailureClass::Retryable);
    assert!(provider_err.detail().contains("event fetch is down"));
    assert_eq!(
        engine.events(&account()).await.unwrap()[0].title,
        "Standup",
        "the store still holds the pre-write copy — which is exactly what Failed says"
    );
}
