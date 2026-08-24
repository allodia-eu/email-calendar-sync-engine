//! Gated live **RSVP** checks against real Microsoft accounts: answering an invitation
//! through `POST /me/events/{id}/tentativelyAccept`, and — the point of the verb —
//! confirming the **organizer was told**.
//!
//! # Why this needs two accounts
//!
//! Graph has no way to fake an invitation. Unlike Google's `events.import`, an event created
//! in a mailbox always has that mailbox as organizer, and a mailbox cannot answer its own
//! meeting (`isOrganizer: true` has no `responseStatus` to move). So a genuine `notResponded`
//! invitation only exists if a *second* account sends one. This suite therefore takes two
//! tokens and skips unless both are present:
//!
//! - `GRAPH_ACCESS_TOKEN` — the account **under test**: it receives the invitation and answers it
//!   through the adapter.
//! - `GRAPH_ORGANIZER_ACCESS_TOKEN` — the counterparty: it creates the meeting and is the mailbox
//!   whose copy proves the answer arrived. Only ever driven by direct HTTP, never by the adapter —
//!   it is the fixture, the same exception `provider-caldav`'s scheduling suite makes for a seeded
//!   document.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token --profile work)" \
//! GRAPH_ORGANIZER_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_calendar_rsvp -- --nocapture
//! ```
//!
//! (`--profile` keeps two accounts signed in side by side: it selects
//! `.local/tokens-<name>.json`.) **This sends real mail** between
//! the two accounts: the invitation, the reply the answer schedules, and a cancellation on
//! cleanup. Both addresses are discovered from the tokens, so nothing is hardcoded, but do
//! not point it at a mailbox whose owner would not expect that.
//!
//! # What only two live accounts can settle
//!
//! 1. **The organizer really is told.** `sendResponse: true` is the entire reason answering is a
//!    verb of its own rather than a patch of the attendee array, and it is unobservable from the
//!    answering mailbox — its own copy changes either way. The assertion here is on the
//!    *organizer's* copy, the same discipline as the CalDAV scheduling suite.
//! 2. **An invitee's copy names the organizer twice.** In the organizer's own copy Graph omits them
//!    from `attendees`; in the invitee's copy it lists them as `organizer` *and* as an
//!    `attendees[]` entry whose `status.response` is `"none"`. A projection that emitted both would
//!    report the person who called the meeting once as accepted and once as not having answered.
//!    Only a delivered invitation shows this shape at all.
//! 3. **`WriteGuard::Absent` is the truth, not a hedge.** The action endpoint takes no
//!    precondition, so a *stale* guard cannot be sent and the answer still lands. Asserted, so that
//!    if Graph ever grows a precondition the capability gets revisited rather than quietly staying
//!    pessimistic.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    ids::{AccountId, CalendarId},
    sync::SyncUpdate,
    time::{CalendarDate, TimeZoneId},
};
use engine_provider::{EventRsvp, Provider, RsvpResponse};
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The answering account's bearer token — the account under test.
fn token() -> Option<String> {
    non_empty("GRAPH_ACCESS_TOKEN")
}

/// The counterparty's token: the mailbox that sends the invitation and whose copy is read
/// back to prove the reply arrived.
fn organizer_token() -> Option<String> {
    non_empty("GRAPH_ORGANIZER_ACCESS_TOKEN")
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|value| !value.is_empty())
}

/// An HTTP client for the fixture calls, built through the engine's own TLS policy.
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

/// The window `calendarView` covers — the seeded meeting sits inside it.
fn calendar_window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

/// The address behind a token, so neither account is hardcoded.
async fn whoami(token: &str) -> String {
    let me: serde_json::Value = http()
        .get(format!("{GRAPH}/me?$select=mail,userPrincipalName"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /me")
        .json()
        .await
        .expect("/me json");
    me["mail"]
        .as_str()
        .or_else(|| me["userPrincipalName"].as_str())
        .expect("the token's own address")
        .to_owned()
}

/// A calendar provider for `token`, bound to that account's default calendar.
async fn calendar_provider(token: &str) -> GraphCalendarProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    let placeholder = GraphCalendarProvider::new(
        client,
        CalendarId::try_from("placeholder").unwrap(),
        calendar_window(),
        TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    );
    let calendars = placeholder
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("a calendar list sync is a snapshot");
    };
    let default = objects
        .iter()
        .find(|calendar| calendar.is_default)
        .expect("a default calendar");
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphCalendarProvider::new(
        client,
        default.id.clone(),
        calendar_window(),
        TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    )
}

/// The counterparty creates a meeting and invites `attendee`. Graph mails the invitation as
/// a side effect — that is what makes the answer below a real RSVP.
///
/// Returns `(the organizer's copy's id, the iCalUId both copies share)`.
async fn seed_invitation(organizer_token: &str, attendee: &str) -> (String, String) {
    let body = serde_json::json!({
        "subject": format!("Live RSVP fixture {} (safe to ignore)", std::process::id()),
        "body": { "contentType": "text",
                  "content": "Automated live test of the engine's RSVP verb. Cancelled at the \
                              end of the run." },
        "start": { "dateTime": "2026-09-25T10:00:00", "timeZone": "Europe/Amsterdam" },
        "end": { "dateTime": "2026-09-25T10:30:00", "timeZone": "Europe/Amsterdam" },
        "attendees": [{ "emailAddress": { "address": attendee }, "type": "required" }],
    });
    let response = http()
        .post(format!("{GRAPH}/me/events"))
        .bearer_auth(organizer_token)
        .json(&body)
        .send()
        .await
        .expect("seed the invitation");
    let status = response.status();
    let created: serde_json::Value = response.json().await.expect("create response");
    assert!(status.is_success(), "seeding failed: {created}");
    (
        created["id"].as_str().expect("an event id").to_owned(),
        created["iCalUId"].as_str().expect("an iCalUId").to_owned(),
    )
}

/// Polls the account's own calendar until the invitation with `uid` is delivered — an
/// invitation crosses two mailboxes, so it is not there the instant the seed returns.
async fn await_invitation(provider: &GraphCalendarProvider, uid: &str) -> Event {
    for _ in 0..20 {
        let sync = provider
            .sync_events(&account(), None)
            .await
            .expect("sync events");
        let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
            panic!("a first events sync is a snapshot");
        };
        if let Some(event) = objects.iter().find(|event| event.uid.as_str() == uid) {
            return event.clone();
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    panic!("the invitation {uid} never arrived in the answering account's calendar");
}

/// Re-reads the account's own copy of `uid`.
async fn reread(provider: &GraphCalendarProvider, uid: &str) -> Event {
    let sync = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("a first events sync is a snapshot");
    };
    objects
        .iter()
        .find(|event| event.uid.as_str() == uid)
        .expect("the account still holds its copy")
        .clone()
}

/// Our own participation status, asserting there is exactly one participant per address.
fn status_of(event: &Event, address: &str) -> ParticipationStatus {
    let matching: Vec<_> = event
        .participants
        .iter()
        .filter(|p| {
            p.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(address))
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one participant per address, not an organizer/attendee pair: {:?}",
        event.participants
    );
    matching[0].participation_status.clone()
}

/// The status the **organizer's** mailbox holds for `attendee`, polled: the reply travels by
/// mail, so the organizer's copy updates a moment after the answer is acknowledged.
async fn organizer_sees(
    organizer_token: &str,
    event_id: &str,
    attendee: &str,
    want: &str,
) -> String {
    let mut last = String::new();
    for _ in 0..20 {
        let event: serde_json::Value = http()
            .get(format!("{GRAPH}/me/events/{event_id}?$select=id,attendees"))
            .bearer_auth(organizer_token)
            .send()
            .await
            .expect("read the organizer's copy")
            .json()
            .await
            .expect("organizer copy json");
        last = event["attendees"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| {
                a["emailAddress"]["address"]
                    .as_str()
                    .is_some_and(|address| address.eq_ignore_ascii_case(attendee))
            })
            .and_then(|a| a["status"]["response"].as_str())
            .unwrap_or("<absent>")
            .to_owned();
        if last == want {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    last
}

/// Leaves both mailboxes as they were found: the organizer cancels (which mails the
/// cancellation), **then** the invitee's copy goes too.
///
/// Both halves are needed. Cancelling only marks the invitee's copy cancelled — Outlook keeps
/// it as a "Cancelled: …" notice — so a run that stopped there would litter the answering
/// calendar a little more every time. Deleting the invitee's copy *first* would be worse: the
/// cancellation lands afterwards and puts the stub back. Best effort throughout, so a cleanup
/// failure never masks the assertion under test.
async fn cleanup(organizer_token: &str, organizer_copy: &str, token: &str, own_copy: &str) {
    for (who, token, id) in [
        ("cancel (organizer)", organizer_token, organizer_copy),
        ("delete (own copy)", token, own_copy),
    ] {
        if who.starts_with("delete") {
            // Give the cancellation time to reach the invitee's calendar, so the delete
            // removes the cancelled item rather than racing it.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        match http()
            .delete(format!("{GRAPH}/me/events/{id}"))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(done) if done.status().is_success() => {}
            Ok(done) => eprintln!("cleanup {who} returned {} (leaving it)", done.status()),
            Err(error) => eprintln!("cleanup {who} failed (leaving it): {error}"),
        }
    }
}

#[tokio::test]
async fn live_rsvp_answers_an_invitation_and_the_organizer_is_told() {
    let (Some(token), Some(organizer_token)) = (token(), organizer_token()) else {
        eprintln!(
            "skipping live_rsvp_answers_an_invitation_...: needs GRAPH_ACCESS_TOKEN *and* \
             GRAPH_ORGANIZER_ACCESS_TOKEN (two accounts — see the module docs)"
        );
        return;
    };
    let me = whoami(&token).await;
    let organizer = whoami(&organizer_token).await;
    assert_ne!(
        me.to_lowercase(),
        organizer.to_lowercase(),
        "the two tokens must be different accounts: a mailbox cannot answer its own meeting"
    );
    let provider = calendar_provider(&token).await;

    let controls = provider
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("Graph advertises that it can answer an invitation");
    assert!(
        controls.comment && controls.suppress_notification,
        "Graph's action endpoint carries a comment and a sendResponse toggle"
    );

    // The counterparty invites us, and Graph delivers it as a real invitation.
    let (organizer_copy, uid) = seed_invitation(&organizer_token, &me).await;
    let invitation = await_invitation(&provider, &uid).await;

    // Finding 2: the organizer is named twice in an invitee's copy and must project once,
    // holding the owner role — and their implied acceptance, not the `"none"` Graph writes
    // for an organizer's own attendee entry.
    assert_eq!(
        status_of(&invitation, &organizer),
        ParticipationStatus::Accepted,
        "the organizer projects as accepted: {:?}",
        invitation.participants
    );
    let owner = invitation
        .participants
        .iter()
        .find(|p| {
            p.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(&organizer))
        })
        .unwrap();
    assert!(owner.has_role(&engine_core::calendar::ParticipantRole::Owner));
    assert!(
        owner.has_role(&engine_core::calendar::ParticipantRole::Attendee),
        "the invitee's copy also lists them as an attendee, and the roles union: {:?}",
        owner.roles
    );
    // And it is genuinely unanswered, so the transition below cannot trivially pass.
    assert_eq!(
        status_of(&invitation, &me),
        ParticipationStatus::NeedsAction,
        "the delivered invitation is unanswered"
    );

    // Answer it — telling the organizer, which is the whole point of the verb.
    provider
        .rsvp_event(
            &account(),
            &invitation,
            &EventRsvp::to(&invitation, &me, RsvpResponse::Tentative).comment("Might be late"),
        )
        .await
        .expect("the neutral verb answers on Graph");

    // Our own copy records it…
    let answered = reread(&provider, &uid).await;
    assert_eq!(
        status_of(&answered, &me),
        ParticipationStatus::Tentative,
        "the answering account's own copy holds the answer"
    );

    // …and, the assertion that needed a second account: so does the ORGANIZER's.
    assert_eq!(
        organizer_sees(
            &organizer_token,
            &organizer_copy,
            &me,
            "tentativelyAccepted"
        )
        .await,
        "tentativelyAccepted",
        "the organizer was told — `sendResponse: true` really schedules the reply"
    );

    // Finding 3: the action endpoint takes no precondition, so answering from a stale read
    // is *not* refused. `WriteGuard::Absent` is observed behaviour, not caution.
    provider
        .rsvp_event(
            &account(),
            &answered,
            // `invitation` is the pre-answer read: its revision is superseded.
            &EventRsvp::to(&invitation, &me, RsvpResponse::Accepted).quietly(),
        )
        .await
        .expect("a stale guard is not refused on Graph — the endpoint has no precondition");

    cleanup(
        &organizer_token,
        &organizer_copy,
        &token,
        invitation.id.key().as_str(),
    )
    .await;
}
