//! Gated live **scheduling** checks against the Stalwart harness: what a JMAP calendar write
//! makes the server tell the other party. Skips with no `STALWART_HTTP_ADDR`.
//!
//! These are the JMAP writes the offline suite structurally cannot judge. An RSVP sends a
//! **JSON-pointer patch** — `participants/<id>/participationStatus` — whose pointer is a map
//! key the engine's projection has already thrown away, so the adapter has to recover it
//! from the preserved `raw_jscalendar`. And whether a `/set` causes an iTIP message at all is
//! a thing the *server* does to a *second account's* copy. The offline `FakeExecutor` answers
//! canned bytes whatever it is sent (`AGENTS.md`), so a pointer that escapes the key wrongly,
//! names a participant that does not exist, or a request that quietly notifies nobody passes
//! every offline test. Only a real server can say.
//!
//! The two-party fixture is [`scheduling`].
//!
//! # Scheduling is opt-in, and this suite used to pin our own omission as a server gap
//!
//! For several months this suite asserted that a JMAP answer *never* reaches the organizer,
//! and attributed it to Stalwart. That was wrong, and the way it was wrong is why these docs
//! are this long.
//!
//! `sendSchedulingMessages` (draft-ietf-jmap-calendars §5.3, default **`false`**) is what
//! makes a server derive an iTIP message from a `/set`. The adapter never sent it. So the
//! server did exactly what it was asked — store the change, tell nobody — and the test
//! recorded that silence as *Stalwart's* behaviour, because the only request shape it ever
//! sent was the one the adapter builds. Isolated on one event, same participant, same
//! server, seconds apart:
//!
//! | attendee RSVPs over JMAP | organizer's copy after 8s |
//! |---|---|
//! | **without** `sendSchedulingMessages` | stays `NEEDS-ACTION` — no `REPLY` |
//! | **with** `sendSchedulingMessages: true` | `DECLINED` — `REPLY` delivered |
//!
//! The CalDAV control arm made it worse rather than better: it "worked", which looked like
//! proof the difference was server-side, when in fact CalDAV auto-schedules per RFC 6638 and
//! has no equivalent opt-in — so the two arms were never comparable. This is the `AGENTS.md`
//! "offline fakes cannot catch a wrong request shape" trap hit *with* a live server, because
//! one shape was all that was ever sent.
//!
//! **The rule that follows, and the reason both directions are tested:** a live test that
//! asserts an absence must first prove the absence is not caused by something we failed to
//! send. [`jmap_rsvp_reaches_the_organizer`] asserts the `REPLY` arrives and
//! [`jmap_a_quiet_answer_reaches_nobody`] asserts it does not when the flag is off, so the
//! flag's effect is pinned from both sides and neither result can be produced by a stuck
//! adapter. [`jmap_cancelling_a_meeting_reaches_the_attendee`] does the same job for the
//! write verbs, which carry the flag unconditionally.
//!
//! The full history is #102 (which inverts #93). The harness pins **v0.16.15**.
//!
//! Every scenario leaves the harness exactly as it found it.

mod scheduling;

use engine_core::{calendar::ParticipationStatus, error::FailureClass};
use engine_provider::{EventDeletion, EventRsvp, Provider, RsvpResponse, WriteGuard};
use scheduling::*;

/// The attendee answers over JMAP: the patch lands, it merges — and the organizer is told.
///
/// The four things only a live server settles:
///
/// 1. Stalwart **accepts** the `participants/<id>/participationStatus` pointer — the id recovered
///    from the preserved JSCalendar really does address a participant it holds.
/// 2. The patch is a **merge**: every other participant, and every property of ours the engine does
///    not model, is left alone. A replace would wipe the organizer off the event.
/// 3. The one control JMAP cannot honour — a note — is refused **before** anything is written.
/// 4. The iTIP `REPLY` reaches the organizer's copy, in another account we never wrote to. This is
///    the assertion the omitted `sendSchedulingMessages` used to make unreachable; dropping the
///    flag from `rsvp_event` turns it red (verified, not assumed).
#[tokio::test]
async fn jmap_rsvp_reaches_the_organizer() {
    let Some(parties) = parties("jmap_rsvp_reaches_the_organizer").await else {
        return;
    };
    clean_up(&parties).await;

    let controls = parties
        .attendee
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("JMAP advertises that it can answer an invitation");
    assert!(
        !controls.comment,
        "no server we run is known to relay a participationComment"
    );
    assert!(
        controls.suppress_notification,
        "JMAP schedules only when the request asks it to, so silence is a real choice here"
    );
    assert_eq!(
        controls.guard,
        WriteGuard::Absent,
        "a CalendarEvent carries no per-object revision, so an RSVP cannot be guarded"
    );

    let mine = deliver_invitation(&parties).await;

    // ---- The control JMAP must refuse, before anything is written. ----
    let refused = parties
        .attendee
        .rsvp_event(
            &attendee_account(),
            &mine,
            &EventRsvp::to(&mine, &parties.attendee_address, RsvpResponse::Accepted)
                .comment("See you there"),
        )
        .await
        .expect_err("asking for a control JMAP cannot honour must fail");
    assert_eq!(
        refused.class(),
        FailureClass::InvalidState,
        "a control this transport cannot honour is a caller error, not a retry"
    );

    // ---- The answer. ----
    parties
        .attendee
        .rsvp_event(
            &attendee_account(),
            &mine,
            &EventRsvp::to(&mine, &parties.attendee_address, RsvpResponse::Tentative),
        )
        .await
        .expect("the neutral verb must answer over JMAP");

    let answered = poll_jmap(&parties, "our own status to read back as tentative", |e| {
        participant(e, &parties.attendee_address) == ParticipationStatus::Tentative
    })
    .await;
    assert!(
        answered
            .participants
            .iter()
            .filter_map(|p| p.email.as_deref())
            .any(|email| engine_core::scheduling::addresses_match(
                email,
                &parties.organizer_address
            )),
        "the patch must MERGE: the organizer is still on the event. A replace would have \
         left an event with one participant — and no organizer to reply to"
    );

    // ---- And the point of the whole exercise. ----
    //
    // The organizer's own copy, in another account, that we never wrote to. This is where the
    // iTIP `REPLY` lands, and it only lands because the `/set` asked for it.
    poll_organizer(
        &parties,
        "the iTIP REPLY to reach the organizer's copy",
        |theirs| participant(theirs, &parties.attendee_address) == ParticipationStatus::Tentative,
    )
    .await;

    clean_up(&parties).await;
}

/// The other direction: `quietly()` stores the answer and tells nobody.
///
/// Without this, [`jmap_rsvp_reaches_the_organizer`] alone would pass just as well if the
/// adapter hard-coded `sendSchedulingMessages: true` and ignored the caller — and the
/// capability would be advertising a toggle that does nothing. It also re-establishes, from
/// the other side, that the silence the old suite recorded was ours to cause: same server,
/// same pointer, flag off, and the organizer hears nothing.
#[tokio::test]
async fn jmap_a_quiet_answer_reaches_nobody() {
    let Some(parties) = parties("jmap_a_quiet_answer_reaches_nobody").await else {
        return;
    };
    clean_up(&parties).await;

    let mine = deliver_invitation(&parties).await;

    parties
        .attendee
        .rsvp_event(
            &attendee_account(),
            &mine,
            &EventRsvp::to(&mine, &parties.attendee_address, RsvpResponse::Declined).quietly(),
        )
        .await
        .expect("a quiet answer is one JMAP can honour");

    // The attendee's own calendar records it...
    poll_jmap(&parties, "our own status to read back as declined", |e| {
        participant(e, &parties.attendee_address) == ParticipationStatus::Declined
    })
    .await;

    // ...and the organizer is not told. An absence, so a fixed settle rather than a poll —
    // and generous, since the notified path above lands well inside it.
    let theirs = organizer_copy_after(&parties, ORGANIZER_SETTLE).await;
    assert_eq!(
        participant(&theirs, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "the user asked for silence; a REPLY here means the flag is being ignored"
    );

    clean_up(&parties).await;
}

/// Cancelling over JMAP reaches the attendee — the write verbs' half of the same flag.
///
/// The RSVP scenarios prove the flag works on an `update`. This one covers `destroy`, and it
/// is the case that motivated carrying the flag on the write verbs at all: without it the
/// organizer's deletion is stored and **every attendee keeps a meeting that is not
/// happening**, with nothing anywhere reporting a failure. That is the same silent-failure
/// shape as the RSVP bug, one verb over.
///
/// The organizer places the invitation over CalDAV (a draft cannot name an attendee) and
/// removes it over **JMAP**, so the cancellation is entirely this adapter's doing. What
/// Stalwart then does to the attendee's copy is observed, not assumed: it may remove the copy
/// or mark it cancelled, and either is a delivered `CANCEL` — what would fail the test is the
/// attendee still holding a live, uncancelled meeting.
#[tokio::test]
async fn jmap_cancelling_a_meeting_reaches_the_attendee() {
    let Some(parties) = parties("jmap_cancelling_a_meeting_reaches_the_attendee").await else {
        return;
    };
    clean_up(&parties).await;

    deliver_invitation(&parties).await;

    // The organizer's own copy, as JMAP sees it — the object the destroy will name.
    let theirs = jmap_events(&parties.organizer_jmap, &organizer_account())
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == parties.uid)
        .expect("the organizer holds their own copy over JMAP too");

    parties
        .organizer_jmap
        .delete_event(&organizer_account(), &EventDeletion::of(&theirs))
        .await
        .expect("the organizer cancels over JMAP");

    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        let copy = attendee_copy(&parties).await;
        let cancelled = match &copy {
            None => true,
            Some(event) => event.status == engine_core::calendar::EventStatus::Cancelled,
        };
        if cancelled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {DELIVERY_TIMEOUT:?}: the attendee still holds a live meeting the \
             organizer cancelled. Without `sendSchedulingMessages` the destroy is stored and \
             no CANCEL is sent, which is exactly this failure"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    clean_up(&parties).await;
}

/// Answering as an address the meeting has no participant for fails — it does not silently
/// add one.
///
/// The alias rule's failure mode, on a real server. An adapter that appended a participant
/// instead would put the user on a meeting the organizer never invited them to, and the
/// organizer would receive a `REPLY` from a stranger.
#[tokio::test]
async fn jmap_answering_as_a_stranger_is_refused() {
    let Some(parties) = parties("jmap_answering_as_a_stranger_is_refused").await else {
        return;
    };
    clean_up(&parties).await;

    let mine = deliver_invitation(&parties).await;

    let refused = parties
        .attendee
        .rsvp_event(
            &attendee_account(),
            &mine,
            &EventRsvp::to(&mine, "nobody@test.local", RsvpResponse::Accepted),
        )
        .await
        .expect_err("you cannot answer an invitation you are not on");
    assert!(
        format!("{refused}").contains("no participant at the answering address"),
        "the error must say which rule was broken; got {refused}"
    );

    let unchanged = attendee_copy(&parties)
        .await
        .expect("the invitation is still there");
    assert_eq!(
        participant(&unchanged, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "and nothing was answered"
    );

    clean_up(&parties).await;
}
