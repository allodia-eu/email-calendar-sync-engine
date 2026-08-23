//! Gated live **RSVP** checks against a real Google account: answering an invitation over
//! `events.patch`, what `sendUpdates` is, and how the answer reads back. Skips unless
//! `GOOGLE_ACCESS_TOKEN` is set.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_calendar_rsvp -- --nocapture
//! ```
//!
//! # Why the invitation is seeded with `events.import`
//!
//! An RSVP is only itself when somebody *else* organizes the meeting, and a single test
//! account cannot be invited by a second party without mailing one. `events.import` is
//! Google's answer to exactly that: it places an event on the calendar with an
//! `organizer` the account is not, and the account as a `needsAction` attendee — no
//! invitation is sent, to anyone, and the `iCalUID` is preserved (`events.insert` mints its
//! own). So the fixture costs no mail and the thing under test is a genuine invitation.
//!
//! The adapter has no import verb and should not grow one — importing is a migration tool,
//! not a sync verb — so the seed is a direct HTTP call, the same exception
//! `provider-caldav`'s scheduling suite makes for a seeded document. **Every assertion is
//! about what the adapter did.**
//!
//! # What only the live API can settle
//!
//! 1. `sendUpdates` is a **query** parameter. In the body it is silently ignored: the patch still
//!    succeeds and the organizer is simply never told. No offline fake can tell those two apart.
//! 2. A **one-element** `attendees` array is read as "this attendee answered" and leaves the other
//!    guests alone — the safety claim `cal_write::rsvp_event` rests on. It is true *for an
//!    attendee*; [`live_rsvp_as_the_organizer_truncates_the_guest_list`] pins the case where it is
//!    not.
//! 3. The answer that reads back. Google names the organizer twice — the `organizer` object and an
//!    `attendees[]` entry — and a projection that emitted both would report the organizer's implied
//!    `accepted` beside the real answer, for the same address. That is not hypothetical: it is the
//!    bug this suite was written to catch, and it made an earlier RSVP test look like a broken
//!    *write* (the status appeared frozen at whatever the read happened to pick) when every patch
//!    had in fact landed.
//!
//! Nothing here emails anybody: every answer is `quietly()` (`sendUpdates=none`) and the
//! seeded organizer is a reserved `example.test` address. Each test cleans up its own event.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    error::FailureClass,
    ids::{CalendarId, EventId, Uid},
    sync::SyncUpdate,
};
use engine_provider::{EventDeletion, EventDraft, EventRsvp, Provider, RsvpResponse};
use provider_google::{GoogleCalendarProvider, GoogleClient};

/// The test account's own address — the attendee every answer here is given as.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";
/// The seeded organizer. A reserved domain that cannot receive mail, so even a mistaken
/// `sendUpdates=all` would reach nobody.
const ORGANIZER: &str = "boss@example.test";

fn account() -> engine_core::ids::AccountId {
    engine_core::ids::AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn calendar() -> CalendarId {
    CalendarId::try_from("primary").unwrap()
}

fn provider(token: String) -> GoogleCalendarProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GoogleCalendarProvider::new(client, calendar())
}

/// An HTTP client for the seed calls, built through the engine's own TLS policy.
///
/// Not `reqwest::Client::new()`: the workspace pins rustls *without* a default crypto
/// provider, so a bare client panics ("no rustls crypto provider is configured") unless
/// something else happens to have installed one first.
fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

fn zoned(local: &str) -> engine_core::time::CalendarDateTime {
    engine_core::time::CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Places an invitation on the account's calendar: organized by [`ORGANIZER`], with the
/// account as a `needsAction` attendee. Returns the event id Google assigned.
///
/// A direct `events.import` call — the fixture step the module docs explain. No mail is
/// sent (import does not schedule) and the `iCalUID` survives verbatim.
async fn import_invitation(token: &str, uid: &Uid, summary: &str) -> EventId {
    let body = serde_json::json!({
        "iCalUID": uid.as_str(),
        "summary": summary,
        "start": { "dateTime": "2026-09-20T10:00:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-09-20T11:00:00", "timeZone": "Europe/Amsterdam" },
        "organizer": { "email": ORGANIZER, "displayName": "The Boss", "self": false },
        "attendees": [
            { "email": SELF_ADDRESS, "self": true, "responseStatus": "needsAction" },
            { "email": ORGANIZER, "displayName": "The Boss", "organizer": true,
              "responseStatus": "accepted" },
        ],
    });
    let response = http()
        .post("https://www.googleapis.com/calendar/v3/calendars/primary/events/import")
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("seed the invitation");
    let status = response.status();
    let imported: serde_json::Value = response.json().await.expect("import response");
    assert!(status.is_success(), "import failed: {imported}");
    EventId::try_from(imported["id"].as_str().expect("an imported event id")).unwrap()
}

/// Creates a meeting the **account itself** organizes, with one other guest — the shape the
/// truncation gap needs, and one `EventDraft` cannot express (a draft states no attendees).
/// `sendUpdates=none`, and the guest is a reserved domain, so nothing is mailed.
async fn insert_with_guest(token: &str, summary: &str) -> EventId {
    let body = serde_json::json!({
        "summary": summary,
        "start": { "dateTime": "2026-09-22T10:00:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-09-22T11:00:00", "timeZone": "Europe/Amsterdam" },
        "attendees": [{ "email": SELF_ADDRESS }, { "email": "guest@example.test" }],
    });
    let response = http()
        .post("https://www.googleapis.com/calendar/v3/calendars/primary/events?sendUpdates=none")
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("seed the meeting");
    let status = response.status();
    let created: serde_json::Value = response.json().await.expect("insert response");
    assert!(status.is_success(), "insert failed: {created}");
    EventId::try_from(created["id"].as_str().expect("an event id")).unwrap()
}

/// Re-reads `id` through the adapter's own sync — the projection a host would see.
async fn read_event(provider: &GoogleCalendarProvider, id: &EventId) -> Event {
    let sync = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a first events sync is a snapshot");
    };
    objects
        .iter()
        .find(|event| &event.id == id)
        .unwrap_or_else(|| panic!("the event {id:?} is on the calendar"))
        .clone()
}

/// Our participation status on `event`, asserting on the way that the projection holds
/// **one** participant for the address — see finding 3 in the module docs.
fn my_status(event: &Event) -> ParticipationStatus {
    let mine: Vec<_> = event
        .participants
        .iter()
        .filter(|p| p.email.as_deref() == Some(SELF_ADDRESS))
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "one participant per address, not an organizer/attendee pair: {:?}",
        event.participants
    );
    mine[0].participation_status.clone()
}

async fn delete(provider: &GoogleCalendarProvider, event: &Event) {
    if let Err(error) = provider
        .delete_event(&account(), &EventDeletion::of(event))
        .await
    {
        eprintln!("cleanup delete gave up (leaving a throwaway event): {error}");
    }
}

/// The whole verb against a genuine invitation: two transitions, each read back, with the
/// other guest still on the event afterwards.
#[tokio::test]
async fn live_rsvp_answers_a_real_invitation_and_leaves_the_other_guest_alone() {
    let Some(token) = token() else {
        eprintln!("skipping live_rsvp_answers_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token.clone());

    let controls = provider
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("Google advertises that it can answer an invitation");
    assert!(
        controls.comment && controls.suppress_notification,
        "Google carries a note and honours sendUpdates=none, unlike the server-scheduled \
         transports"
    );

    // A pid-based uid keeps concurrent runs on this shared account from colliding.
    let uid = Uid::new(format!("live-rsvp-{}@example.test", std::process::id())).unwrap();
    let id = import_invitation(&token, &uid, "Live RSVP: invitation").await;

    // The seed really is an unanswered invitation organized by somebody else.
    let invited = read_event(&provider, &id).await;
    assert_eq!(
        my_status(&invited),
        ParticipationStatus::NeedsAction,
        "the seeded invitation is unanswered — otherwise the transition below cannot fail"
    );
    assert_eq!(uid, invited.uid, "events.import preserves the caller's uid");

    // Answer it. `quietly()` is what makes this safe to run: Google would email the
    // organizer otherwise, and that email is the point of the verb existing.
    provider
        .rsvp_event(
            &account(),
            &invited,
            &EventRsvp::to(&invited, SELF_ADDRESS, RsvpResponse::Tentative)
                .comment("might be late")
                .quietly(),
        )
        .await
        .expect("the neutral verb answers on Google");

    let answered = read_event(&provider, &id).await;
    assert_eq!(
        my_status(&answered),
        ParticipationStatus::Tentative,
        "Google recorded the responseStatus the one-element attendees patch carried"
    );
    // The other guest survived a one-element `attendees` array — the claim
    // `cal_write::rsvp_event` rests on, and the reason it does not rebuild the array from
    // the projection.
    let organizer = answered
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some(ORGANIZER))
        .expect("the organizer is still on the event after the answer");
    assert_eq!(
        organizer.participation_status,
        ParticipationStatus::Accepted
    );

    // A second transition, from the revision the first answer produced: nothing about the
    // first answer makes the next one a no-op.
    provider
        .rsvp_event(
            &account(),
            &answered,
            &EventRsvp::to(&answered, SELF_ADDRESS, RsvpResponse::Declined).quietly(),
        )
        .await
        .expect("answering again is accepted");
    let declined = read_event(&provider, &id).await;
    assert_eq!(my_status(&declined), ParticipationStatus::Declined);

    // And the guard is real: replaying the *stale* revision is refused as a Conflict, which
    // is what tells the outbox to refetch rather than retry blindly.
    let error = provider
        .rsvp_event(
            &account(),
            &declined,
            &EventRsvp::to(&invited, SELF_ADDRESS, RsvpResponse::Accepted).quietly(),
        )
        .await
        .expect_err("a stale guard must be refused");
    assert_eq!(
        error.class(),
        FailureClass::Conflict,
        "a superseded ETag is recoverable by refetch, not permanent: {error:?}"
    );

    delete(&provider, &declined).await;
}

/// The read-back on an event the account **organizes**, where Google names it twice.
///
/// This is the shape that misled an earlier version of this suite: with a separate
/// participant synthesized from `organizer` (fixed at `accepted`) beside the real attendee
/// entry, a host looking its own address up read `accepted` however it had answered.
#[tokio::test]
async fn live_rsvp_on_my_own_meeting_reads_back_as_one_participant() {
    let Some(token) = token() else {
        eprintln!("skipping live_rsvp_on_my_own_meeting_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token.clone());
    let stamp: engine_core::time::UtcDateTime = "2026-07-31T10:00:00Z".parse().unwrap();

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(),
                Uid::new(format!("live-own-rsvp-{}@example.test", std::process::id())).unwrap(),
                "Live RSVP: my own meeting",
                zoned("2026-09-21T10:00:00"),
                zoned("2026-09-21T11:00:00"),
                stamp,
            ),
        )
        .await
        .expect("create");

    // A draft states no attendees, so answering is what puts the account on its own event —
    // and Google accepts a patch naming an address the event did not carry yet, creating
    // the attendee. That is the same request the RSVP verb sends, so it is under test too.
    let mut base = read_event(&provider, &created.event).await;
    provider
        .rsvp_event(
            &account(),
            &base,
            &EventRsvp::to(&base, SELF_ADDRESS, RsvpResponse::Declined).quietly(),
        )
        .await
        .expect("answer my own meeting");

    base = read_event(&provider, &created.event).await;
    // One participant, both roles, and the status the server holds — not the `accepted` the
    // organizer object implies.
    assert_eq!(my_status(&base), ParticipationStatus::Declined);
    let me = &base.participants[0];
    assert!(
        me.roles
            .contains(&engine_core::calendar::ParticipantRole::Owner),
        "the organizer role survives the merge: {:?}",
        me.roles
    );
    assert!(
        me.roles
            .contains(&engine_core::calendar::ParticipantRole::Attendee),
        "and so does the attendee role: {:?}",
        me.roles
    );

    delete(&provider, &base).await;
}

/// The gap `cal_write::rsvp_event` documents, pinned rather than assumed: when the
/// **caller is the organizer**, Google stops merging and lets the one-element array
/// *replace* the guest list — so answering your own meeting drops the other guests.
///
/// Google's leniency is keyed on the caller's role, not on the array: as an attendee (the
/// test above) the other guests survive; as the organizer they do not. Asserted because a
/// host must be kept off this path by policy, and because if Google ever changes it the
/// adapter's known gap should be revisited rather than quietly outlive it.
#[tokio::test]
async fn live_rsvp_as_the_organizer_truncates_the_guest_list() {
    let Some(token) = token() else {
        eprintln!("skipping live_rsvp_as_the_organizer_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token.clone());

    // A meeting the account organizes, with a second guest. Seeded directly because a draft
    // states no attendees; `sendUpdates=none`, and the guest is a reserved domain, so no
    // mail is sent either way.
    let id = insert_with_guest(&token, "Live RSVP: organizer truncation").await;
    let seeded = read_event(&provider, &id).await;
    assert_eq!(
        seeded.participants.len(),
        2,
        "the precondition: there is a guest to lose {:?}",
        seeded.participants
    );

    provider
        .rsvp_event(
            &account(),
            &seeded,
            &EventRsvp::to(&seeded, SELF_ADDRESS, RsvpResponse::Declined).quietly(),
        )
        .await
        .expect("Google accepts the organizer answering their own invitation");

    let after = read_event(&provider, &id).await;
    assert_eq!(
        after.participants.len(),
        1,
        "as the organizer, the one-element array REPLACED the guest list — the documented \
         gap. If this ever holds 2, Google changed and the gap can be closed: {:?}",
        after.participants
    );
    assert_eq!(
        after.participants[0].email.as_deref(),
        Some(SELF_ADDRESS),
        "and the survivor is the attendee the patch named"
    );

    delete(&provider, &after).await;
}
