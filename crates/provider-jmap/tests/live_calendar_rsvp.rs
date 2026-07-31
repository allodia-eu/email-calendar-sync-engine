//! Gated live **RSVP** checks against the Stalwart harness: answering an invitation over
//! JMAP, and the iTIP `REPLY` the server derives from it. Skips with no `STALWART_HTTP_ADDR`.
//!
//! This is the one JMAP write the offline suite structurally cannot judge. `rsvp_event`
//! sends a **JSON-pointer patch** — `participants/<id>/participationStatus` — whose pointer
//! is a map key the engine's projection has already thrown away, so the adapter has to
//! recover it from the preserved `raw_jscalendar`. The offline `FakeExecutor` answers canned
//! bytes whatever it is sent (`AGENTS.md`), so a pointer that escapes the key wrongly, names
//! a participant that does not exist, or patches a property Stalwart refuses passes every
//! offline test. Only a real server can say.
//!
//! # Why the invitation is seeded over CalDAV
//!
//! JMAP has no whole-document write, and `EventDraft` cannot state an `ORGANIZER`/`ATTENDEE`
//! pair — so there is no way to *create* an invitation over JMAP alone. The organizer's copy
//! is therefore placed with a CalDAV `PUT`, exactly as `provider-caldav`'s scheduling
//! fixture does, and Stalwart's RFC 6638 auto-scheduling delivers it to the attendee. That
//! is the counterparty's fixture, not the thing under test: **every assertion here is about
//! what JMAP did**.
//!
//! It also makes the test stronger than a same-protocol one would be. The invitation arrives
//! by one protocol and is answered by another, against one server — so the participant ids
//! the JMAP projection sees really do address the resource CalDAV wrote, and the two
//! protocols' scheduling behaviour can be compared with everything else held constant.
//!
//! # Stalwart does not schedule a `REPLY` from a JMAP answer
//!
//! Which is what that comparison found, and it is the reason this file exists. With the
//! same two accounts, the same invitation and the same neutral verb, minutes apart:
//!
//! | answered over | the attendee's own copy | the **organizer's** copy |
//! |---|---|---|
//! | CalDAV (`PARTSTAT` in a `PUT`) | changes | **the `REPLY` arrives**, in under a second |
//! | JMAP (`participationStatus` patch) | changes | **never changes** |
//!
//! So on this server a JMAP RSVP is stored and *goes nowhere*: the user answers, their own
//! calendar agrees, and the organizer is never told. That is worse than being unable to
//! answer, because nothing on either side says it failed — the exact silent failure the
//! RSVP design is built to make unreachable, arriving from the server rather than from us.
//!
//! The adapter is **not** at fault, which is why it is unchanged: the patch lands, it merges,
//! and the wrong-address case is refused. What is missing is server-side scheduling that
//! JMAP Calendars leaves to the implementation.
//!
//! [`jmap_rsvp_stores_the_answer_but_the_organizer_is_never_told`] therefore asserts the
//! **observed** behaviour, including the absence — the same discipline as
//! `a_stale_edit_is_not_refused` in the sibling write suite, which pins
//! [`WriteGuard::Absent`] to what Stalwart actually does rather than to a reading of the
//! spec. If Stalwart starts scheduling, that test fails, and the capability and the known
//! gap get revisited rather than the test quietly relaxed.
//!
//! Every scenario leaves the harness exactly as it found it.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    error::FailureClass,
    ids::{AccountId, Uid},
    raw::RawIcal,
    scheduling::addresses_match,
    sync::SyncUpdate,
};
use engine_provider::{EventDeletion, EventRsvp, EventWrite, Provider, RsvpResponse, WriteGuard};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials as DavCredentials};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, ScratchAccount};

const RSVP_UID: &str = "jmap-rsvp-verb@test.local";

/// How long to wait for an asynchronous iTIP delivery. A poll on real state, never a sleep.
const DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long the server gets to schedule a `REPLY` before we record that it did not.
///
/// The CalDAV path lands in well under a second, so this is generous by an order of
/// magnitude — an absence claimed too early would be a flake, and this one is load-bearing.
const ORGANIZER_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

fn organizer_account() -> AccountId {
    AccountId::try_from("jmap-rsvp-organizer").unwrap()
}

fn attendee_account() -> AccountId {
    AccountId::try_from("jmap-rsvp-attendee").unwrap()
}

/// The two parties: the organizer speaks CalDAV (it only has to *place* the invitation), the
/// attendee speaks JMAP (it is the one under test).
struct Parties {
    organizer: CalDavProvider,
    organizer_address: String,
    attendee: JmapProvider,
    attendee_address: String,
}

async fn parties(test: &str) -> Option<Parties> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let base = format!("http://{}", harness.http_addr);
    let [organizer_auth, attendee_auth] = harness.scratch.clone();

    let organizer = CalDavProvider::connect(CalDavConfig::new(
        base.clone(),
        DavCredentials::Basic {
            username: organizer_auth.address.clone(),
            password: organizer_auth.password.clone(),
        },
    ))
    .await
    .expect("organizer connects over CalDAV");

    let attendee = connect_jmap(&base, &attendee_auth).await;

    Some(Parties {
        organizer_address: organizer_auth.address,
        attendee_address: attendee_auth.address.clone(),
        organizer,
        attendee,
    })
}

async fn connect_jmap(base: &str, account: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        base.to_owned(),
        Credentials::basic(&account.address, &account.password),
    ))
    .await
    .expect("attendee connects over JMAP")
}

/// The organizer's invitation document. Assembled by hand for the same reason the CalDAV
/// fixture does it: a draft cannot state an `ORGANIZER`/`ATTENDEE` pair. Fixed 2026 dates,
/// per the determinism rule.
fn invitation(parties: &Parties) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Harness//JMAP RSVP//EN\r\n\
         BEGIN:VEVENT\r\nUID:{RSVP_UID}\r\nDTSTAMP:20260701T080000Z\r\nSEQUENCE:0\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260812T140000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260812T150000\r\n\
         SUMMARY:RSVP over JMAP\r\n\
         ORGANIZER;CN=Bob Tester:mailto:{organizer}\r\n\
         ATTENDEE;CN=Carol;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:\
         {attendee}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        organizer = parties.organizer_address,
        attendee = parties.attendee_address,
    )
}

/// Every event an account currently holds, over whichever protocol it speaks.
async fn jmap_events(provider: &JmapProvider, account: &AccountId) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(account, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("expected a snapshot");
    };
    objects
}

async fn dav_events(provider: &CalDavProvider, account: &AccountId) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(account, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("expected a snapshot");
    };
    objects
}

/// The participant whose calendar address is `address`, by the one shared comparison.
fn participant(event: &Event, address: &str) -> ParticipationStatus {
    event
        .participants
        .iter()
        .find(|p| {
            p.email
                .as_deref()
                .is_some_and(|email| addresses_match(email, address))
        })
        .unwrap_or_else(|| panic!("a participant for {address}: {:?}", event.participants))
        .participation_status
        .clone()
}

/// Polls the attendee's JMAP calendar until `ready` accepts the event, or fails.
async fn poll_jmap(parties: &Parties, what: &str, ready: impl Fn(&Event) -> bool) -> Event {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if let Some(event) = jmap_events(&parties.attendee, &attendee_account())
            .await
            .into_iter()
            .find(|e| e.uid.as_str() == RSVP_UID)
            && ready(&event)
        {
            return event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {DELIVERY_TIMEOUT:?} waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The organizer's CalDAV copy after giving the server `settle` to schedule anything it
/// meant to — a different account's resource we never wrote to.
///
/// A fixed wait rather than a poll, because this one asserts an **absence**: there is no
/// state to poll toward, and a poll would only return early on the outcome we are recording
/// does not happen. The CalDAV equivalent lands in well under a second, so this is generous
/// by an order of magnitude.
async fn organizer_copy_after(parties: &Parties, settle: std::time::Duration) -> Event {
    tokio::time::sleep(settle).await;
    dav_events(&parties.organizer, &organizer_account())
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == RSVP_UID)
        .expect("the organizer still holds their own copy")
}

/// Removes both parties' copies — an attendee's copy outlives the organizer's on an
/// auto-schedule server, so cleanup has to delete each side.
async fn clean_up(parties: &Parties) {
    for event in dav_events(&parties.organizer, &organizer_account())
        .await
        .into_iter()
        .filter(|e| e.uid.as_str() == RSVP_UID)
    {
        let _ = parties
            .organizer
            .delete_event(&organizer_account(), &EventDeletion::of(&event))
            .await;
    }
    for event in jmap_events(&parties.attendee, &attendee_account())
        .await
        .into_iter()
        .filter(|e| e.uid.as_str() == RSVP_UID)
    {
        let _ = parties
            .attendee
            .delete_event(&attendee_account(), &EventDeletion::of(&event))
            .await;
    }
}

/// The attendee answers over JMAP: the patch lands and merges — and the organizer is never
/// told.
///
/// The four things only a live server settles, the last of them the finding in the module
/// docs:
///
/// 1. Stalwart **accepts** the `participants/<id>/participationStatus` pointer — the id recovered
///    from the preserved JSCalendar really does address a participant it holds.
/// 2. The patch is a **merge**: every other participant, and every property of ours the engine does
///    not model, is left alone. A replace would wipe the organizer off the event.
/// 3. The two controls JMAP cannot honour are refused **before** anything is written.
/// 4. No iTIP `REPLY` reaches the organizer — asserted as an absence, deliberately. Read the module
///    docs before touching that assertion.
#[tokio::test]
async fn jmap_rsvp_stores_the_answer_but_the_organizer_is_never_told() {
    let Some(parties) =
        parties("jmap_rsvp_stores_the_answer_but_the_organizer_is_never_told").await
    else {
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
        !controls.comment && !controls.suppress_notification,
        "JMAP has no per-answer note and no way to suppress the server's REPLY"
    );
    assert_eq!(
        controls.guard,
        WriteGuard::Absent,
        "a CalendarEvent carries no per-object revision, so an RSVP cannot be guarded"
    );

    // ---- The organizer places the invitation; the server delivers it. ----
    let href = parties
        .organizer
        .event_href(&Uid::new(RSVP_UID).unwrap())
        .expect("mint event href");
    parties
        .organizer
        .put_event(
            &organizer_account(),
            &EventWrite::unconditional(
                href,
                Uid::new(RSVP_UID).unwrap(),
                RawIcal::new(invitation(&parties)),
            ),
        )
        .await
        .expect("the organizer stores the invitation");

    let mine = poll_jmap(
        &parties,
        "the invitation to reach the attendee over JMAP",
        |_| true,
    )
    .await;
    assert_eq!(
        participant(&mine, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "the delivered invitation must start unanswered, or this proves nothing"
    );

    // ---- The two controls JMAP must refuse, before anything is written. ----
    for (label, rsvp) in [
        (
            "a note",
            EventRsvp::to(&mine, &parties.attendee_address, RsvpResponse::Accepted)
                .comment("See you there"),
        ),
        (
            "silence",
            EventRsvp::to(&mine, &parties.attendee_address, RsvpResponse::Accepted).quietly(),
        ),
    ] {
        let refused = parties
            .attendee
            .rsvp_event(&attendee_account(), &mine, &rsvp)
            .await
            .expect_err("asking for a control JMAP cannot honour must fail");
        assert_eq!(
            refused.class(),
            FailureClass::InvalidState,
            "{label}: a control this transport cannot honour is a caller error, not a retry"
        );
    }

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
            .any(|email| addresses_match(email, &parties.organizer_address)),
        "the patch must MERGE: the organizer is still on the event. A replace would have \
         left an event with one participant — and no organizer to reply to"
    );

    // ---- And now the finding. ----
    //
    // The organizer's own copy, in another account, that we never wrote to. On CalDAV this
    // is where the iTIP `REPLY` shows up, within a second
    // (`provider-caldav`'s `an_rsvp_through_the_neutral_verb_reaches_the_organizer`). Over
    // JMAP, against this server, it never does.
    let theirs = organizer_copy_after(&parties, ORGANIZER_SETTLE).await;
    assert_eq!(
        participant(&theirs, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "OBSERVED, NOT DESIRED — see this test's `# Stalwart does not schedule a REPLY from \
         a JMAP answer` docs. If this assertion fails, Stalwart has started scheduling and \
         the known gap must be re-examined, not the test 'fixed'."
    );

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

    let href = parties
        .organizer
        .event_href(&Uid::new(RSVP_UID).unwrap())
        .expect("mint event href");
    parties
        .organizer
        .put_event(
            &organizer_account(),
            &EventWrite::unconditional(
                href,
                Uid::new(RSVP_UID).unwrap(),
                RawIcal::new(invitation(&parties)),
            ),
        )
        .await
        .expect("the organizer stores the invitation");
    let mine = poll_jmap(&parties, "the invitation to reach the attendee", |_| true).await;

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

    let unchanged = jmap_events(&parties.attendee, &attendee_account())
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == RSVP_UID)
        .expect("the invitation is still there");
    assert_eq!(
        participant(&unchanged, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "and nothing was answered"
    );

    clean_up(&parties).await;
}
