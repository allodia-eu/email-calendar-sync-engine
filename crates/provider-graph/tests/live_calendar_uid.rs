//! Gated live check that a meeting keeps **one identity** across the mail that announced it
//! and the calendar that filed it.
//!
//! An invitation arrives twice: as an iMIP message carrying a `UID`, and as a calendar item
//! the server files by itself. Answering the first means writing to the second, and the only
//! thing joining them is that `UID` (RFC 5546 §2.1.5). On Exchange that join is not free:
//! Exchange re-encodes an outside `UID` into a `PidLidGlobalObjectId` and reports *that* as
//! `iCalUId`, while `uid` keeps the organizer's own. An adapter reading the wrapper hands the
//! host two names for one meeting, and every answer to an emailed invitation fails to find
//! the event it is about.
//!
//! # Why an invitation this suite builds itself
//!
//! `live_calendar_rsvp.rs` seeds its invitation with `POST /me/events`, which is an
//! *Exchange* organizer: the `UID` is Exchange's own and the wrapping never happens, so that
//! suite cannot see this at all. The wrapping needs a `UID` from outside, which means an iMIP
//! message this suite assembles and sends as MIME — the same shape a CalDAV server mails.
//!
//! Two tokens, like the RSVP suite, and skipped unless both are present:
//!
//! - `GRAPH_ACCESS_TOKEN` — the account **under test**: it receives the invitation, and its
//!   calendar is read through the adapter.
//! - `GRAPH_ORGANIZER_ACCESS_TOKEN` — the counterparty, used only to post the message. It is the
//!   transport, not the organizer: the `ORGANIZER` inside the iCalendar object is an `example.com`
//!   address, which is what makes the `UID` an outside one.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token --profile work)" \
//! GRAPH_ORGANIZER_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_calendar_uid -- --nocapture
//! ```
//!
//! **This sends real mail** between the two accounts and files a meeting in the account under
//! test's calendar; both are cleaned up at the end.

use engine_core::{ids::AccountId, sync::SyncUpdate, time::CalendarDate};
use engine_provider::Provider;
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// The key the adapter preserves the whole raw Graph event under.
const RAW_EVENT: &str = "microsoft.graph/event";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|value| !value.is_empty())
}

/// An HTTP client for the fixture calls, on the engine's own TLS policy (a bare
/// `reqwest::Client` panics: the workspace pins rustls without a default crypto provider).
fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

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
        engine_core::ids::CalendarId::try_from("placeholder").unwrap(),
        calendar_window(),
        engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
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
        .expect("a default calendar")
        .id
        .clone();
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphCalendarProvider::new(
        client,
        default,
        calendar_window(),
        engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    )
}

/// The iMIP `REQUEST` an organizer outside Exchange sends: `multipart/alternative` with a
/// `text/calendar; method=REQUEST` part, which is what makes Exchange file the meeting.
fn imip_request(sender: &str, attendee: &str, uid: &str, subject: &str) -> String {
    let ics = [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//engine live test//EN",
        "METHOD:REQUEST",
        "BEGIN:VEVENT",
        &format!("UID:{uid}"),
        "DTSTART:20261002T090000Z",
        "DTEND:20261002T093000Z",
        "DTSTAMP:20260901T120000Z",
        &format!("SUMMARY:{subject}"),
        "ORGANIZER;CN=Live test:mailto:organizer@example.com",
        &format!("ATTENDEE;CN={attendee};RSVP=TRUE;PARTSTAT=NEEDS-ACTION:mailto:{attendee}"),
        "SEQUENCE:0",
        "END:VEVENT",
        "END:VCALENDAR",
        "",
    ]
    .join("\r\n");
    [
        &format!("From: {sender}"),
        &format!("To: {attendee}"),
        &format!("Subject: {subject}"),
        "MIME-Version: 1.0",
        "Content-Type: multipart/alternative; boundary=\"engine-live\"",
        "",
        "--engine-live",
        "Content-Type: text/plain; charset=\"utf-8\"",
        "",
        "An automated engine live test. Safe to ignore.",
        "",
        "--engine-live",
        "Content-Type: text/calendar; charset=\"utf-8\"; method=\"REQUEST\"",
        "",
        &ics,
        "--engine-live--",
        "",
    ]
    .join("\r\n")
}

/// Posts the invitation as MIME, the one `sendMail` form that carries a `text/calendar` part
/// through untouched.
async fn send_invitation(organizer_token: &str, mime: &str) {
    let response = http()
        .post(format!("{GRAPH}/me/sendMail"))
        .bearer_auth(organizer_token)
        .header("Content-Type", "text/plain")
        .body(engine_rfc5322::base64_encode(mime.as_bytes()))
        .send()
        .await
        .expect("post the invitation");
    let status = response.status();
    assert!(
        status.is_success(),
        "sendMail failed: {status} {}",
        response.text().await.unwrap_or_default()
    );
}

/// Polls the account's calendar through the adapter until the meeting is filed — it crosses
/// two mailboxes and is then processed by the receiving one, so it is not there at once.
async fn await_filed(
    provider: &GraphCalendarProvider,
    subject: &str,
) -> engine_core::calendar::Event {
    for _ in 0..20 {
        let sync = provider
            .sync_events(&account(), None)
            .await
            .expect("sync events");
        let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
            panic!("a first events sync is a snapshot");
        };
        if let Some(event) = objects.iter().find(|event| event.title == subject) {
            return event.clone();
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    panic!("the invitation was never filed in the receiving account's calendar");
}

/// Best effort, so a cleanup failure never masks the assertion: the filed meeting, then the
/// message that carried it.
async fn cleanup(token: &str, event_id: &str, subject: &str) {
    let deleted = http()
        .delete(format!("{GRAPH}/me/events/{event_id}"))
        .bearer_auth(token)
        .send()
        .await;
    if let Ok(response) = deleted
        && !response.status().is_success()
    {
        eprintln!("cleanup: deleting the event returned {}", response.status());
    }
    let found: Result<serde_json::Value, _> = async {
        http()
            .get(format!(
                "{GRAPH}/me/messages?$filter=subject eq '{subject}'&$select=id"
            ))
            .bearer_auth(token)
            .send()
            .await?
            .json()
            .await
    }
    .await;
    for id in found
        .iter()
        .filter_map(|page| page["value"].as_array())
        .flatten()
        .filter_map(|message| message["id"].as_str())
    {
        let _ = http()
            .delete(format!("{GRAPH}/me/messages/{id}"))
            .bearer_auth(token)
            .send()
            .await;
    }
}

#[tokio::test]
async fn live_an_invitation_from_outside_exchange_keeps_its_own_uid() {
    let (Some(token), Some(organizer_token)) = (
        non_empty("GRAPH_ACCESS_TOKEN"),
        non_empty("GRAPH_ORGANIZER_ACCESS_TOKEN"),
    ) else {
        eprintln!(
            "skipping live_an_invitation_from_outside_exchange_...: needs GRAPH_ACCESS_TOKEN \
             *and* GRAPH_ORGANIZER_ACCESS_TOKEN (two accounts — see the module docs)"
        );
        return;
    };
    let me = whoami(&token).await;
    let sender = whoami(&organizer_token).await;
    assert_ne!(
        me.to_lowercase(),
        sender.to_lowercase(),
        "the two tokens must be different accounts: a mailbox does not mail itself an invitation"
    );

    // Unique per run, so a previous run's copy can never be the one this one asserts on.
    let run = std::process::id();
    let uid = format!("engine-live-{run}@example.com");
    let subject = format!("Engine live UID probe {run}");
    send_invitation(
        &organizer_token,
        &imip_request(&sender, &me, &uid, &subject),
    )
    .await;

    let provider = calendar_provider(&token).await;
    let filed = await_filed(&provider, &subject).await;

    // The claim: the meeting the calendar holds is the meeting the mail announced.
    assert_eq!(
        filed.uid.as_str(),
        uid,
        "the filed meeting must carry the organizer's own UID"
    );

    // And the wrapper Exchange built is genuinely a different string, so the assertion above
    // cannot pass because Exchange happened to keep the UID as it arrived. This is the whole
    // reason the adapter must not read `iCalUId`.
    let raw = filed
        .extended
        .get(RAW_EVENT)
        .expect("the raw Graph event is preserved beside the projection");
    let ical_uid = raw["iCalUId"].as_str().expect("an iCalUId");
    assert_ne!(
        ical_uid, uid,
        "Exchange re-encodes an outside UID; if it stopped, this suite is no longer testing \
         anything"
    );
    assert!(
        ical_uid.contains(&hex_upper(uid.as_bytes())),
        "the wrapper embeds the UID it re-encoded: {ical_uid}"
    );

    cleanup(&token, filed.id.key().as_str(), &subject).await;
}

/// The uppercase hex of `bytes`, the form Exchange renders a `PidLidGlobalObjectId` in.
fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02X}");
        out
    })
}
