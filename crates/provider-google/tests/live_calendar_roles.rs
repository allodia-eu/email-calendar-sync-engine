//! Gated live proof of the `accessRole` → `CalendarAccess` mapping for the roles an account can
//! only hold on **someone else's** calendar: `writer`, `writerWithoutPrivateAccess`, `reader`
//! and `freeBusyReader`. (`owner` is proven on the throwaway's own primary, `live_calendar.rs`.)
//!
//! Two accounts. `GOOGLE_ACCESS_TOKEN` is the grantee, the throwaway every other suite uses.
//! `GOOGLE_SHARER_ACCESS_TOKEN` is a second account that owns the calendars, and needs only the
//! calendar scope:
//!
//! ```sh
//! GOOGLE_TOKENS=tools/google-oauth/.local/tokens-sharer.json \
//!   cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- login --client-id … \
//!   --client-secret … --scopes "https://www.googleapis.com/auth/calendar openid email"
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//! GOOGLE_SHARER_ACCESS_TOKEN="$(GOOGLE_TOKENS=tools/google-oauth/.local/tokens-sharer.json \
//!   cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_calendar_roles -- --nocapture
//! ```
//!
//! A label is not a grant, so the engine's reading of each role is held against what Google
//! lets the grantee do on that calendar: read an event's details, see its busy time, add an
//! event, share the calendar, delete it. The same five probes run on every role, so each
//! refusal is pinned against a role on which the identical request succeeds — a probe that
//! could never succeed would show up as a mismatch there, not pass as a refusal.
//!
//! The sharer's calendars exist only for the run: created at the start, after deleting any an
//! interrupted run left behind, and deleted at the end. The grants send no notification.

mod common;

use common::*;
use engine_core::{calendar::CalendarAccess, sync::SyncUpdate};
use engine_provider::Provider;
use reqwest::Method;
use serde_json::{Value, json};

const API: &str = "https://www.googleapis.com/calendar/v3";
/// Every probe calendar's name starts with this, which is how leftovers are found.
const PREFIX: &str = "engine live role probe: ";
const ROLES: [&str; 4] = [
    "writer",
    "writerWithoutPrivateAccess",
    "reader",
    "freeBusyReader",
];
const PROBE_EVENT: &str = "role probe event";
const START: &str = "2027-01-15T10:00:00Z";
const END: &str = "2027-01-15T11:00:00Z";

/// One account's side of the conversation: raw calendar API calls, answered with the status
/// and whatever JSON came back.
struct Api {
    http: reqwest::Client,
    token: String,
}

impl Api {
    fn new(token: String) -> Self {
        // A bare `reqwest::Client` panics: the workspace pins rustls without a default provider.
        let http = engine_tls::TlsClientConfig::bundled()
            .reqwest_builder()
            .build()
            .expect("an HTTP client on the engine's TLS policy");
        Self { http, token }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("a response");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// What Google let the grantee do on one calendar.
// One flag per `CalendarAccess` permission it is held against, which is why that type
// allows the same lint.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Observed {
    read: bool,
    free_busy: bool,
    write: bool,
    share: bool,
    delete: bool,
}

impl Observed {
    /// Everything either observation saw allowed.
    fn or(self, other: Self) -> Self {
        Self {
            read: self.read || other.read,
            free_busy: self.free_busy || other.free_busy,
            write: self.write || other.write,
            share: self.share || other.share,
            delete: self.delete || other.delete,
        }
    }
}

/// Probes one calendar until two rounds agree, and reports every permission any round saw.
///
/// A grant reaches event reads some time after the calendar list shows it: one run was refused
/// an event it could read moments later. So a refusal may be a grant still arriving, while a
/// success never is — nothing is revoked during the run — and a success is kept once seen. That
/// also keeps a destructive probe that should have failed from hiding: a calendar the grantee
/// managed to delete answers `404` to every later round, but the deletion stays on the record.
/// Agreement between rounds, never the engine's answer, is what ends the wait.
async fn settled(grantee: &Api, calendar: &str, event: &str, grantee_email: &str) -> Observed {
    let mut last = probe(grantee, calendar, event, grantee_email).await;
    let mut seen = last;
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let next = probe(grantee, calendar, event, grantee_email).await;
        seen = seen.or(next);
        if next == last {
            break;
        }
        last = next;
    }
    seen
}

/// The grantee's side of one calendar, probed in the order that keeps a probe from spoiling
/// the next: the destructive two last, deletion very last.
async fn probe(grantee: &Api, calendar: &str, event: &str, grantee_email: &str) -> Observed {
    let (status, body) = grantee
        .call(
            Method::GET,
            &format!("/calendars/{calendar}/events/{event}"),
            None,
        )
        .await;
    // Not merely a `200`: a free/busy reader's `events.get` answers `200` too, with the times
    // alone — no summary, no creator. Reading an event means seeing what it is.
    let read = status == 200 && body["summary"] == PROBE_EVENT;

    let query = json!({
        "timeMin": "2027-01-15T00:00:00Z",
        "timeMax": "2027-01-16T00:00:00Z",
        "items": [{ "id": calendar }],
    });
    let (_, busy) = grantee.call(Method::POST, "/freeBusy", Some(query)).await;
    let free_busy = busy["calendars"][calendar]["busy"]
        .as_array()
        .is_some_and(|slots| !slots.is_empty());

    let added = json!({
        "summary": "written by the grantee",
        "start": { "dateTime": START },
        "end": { "dateTime": END },
    });
    let (status, created) = grantee
        .call(
            Method::POST,
            &format!("/calendars/{calendar}/events"),
            Some(added),
        )
        .await;
    let write = status == 200;
    if let Some(id) = created["id"].as_str() {
        grantee
            .call(
                Method::DELETE,
                &format!("/calendars/{calendar}/events/{id}"),
                None,
            )
            .await;
    }

    // Harmless if it succeeds — it re-grants the grantee a role on a calendar about to go.
    let grant = json!({ "role": "reader", "scope": { "type": "user", "value": grantee_email } });
    let (status, _) = grantee
        .call(
            Method::POST,
            &format!("/calendars/{calendar}/acl?sendNotifications=false"),
            Some(grant),
        )
        .await;
    let share = status == 200;

    let (status, _) = grantee
        .call(Method::DELETE, &format!("/calendars/{calendar}"), None)
        .await;
    let delete = status == 204;

    Observed {
        read,
        free_busy,
        write,
        share,
        delete,
    }
}

/// Deletes every probe calendar the sharer owns — this run's, or an interrupted run's.
async fn delete_probe_calendars(sharer: &Api) {
    let (_, list) = sharer
        .call(
            Method::GET,
            "/users/me/calendarList?minAccessRole=owner",
            None,
        )
        .await;
    for entry in list["items"].as_array().into_iter().flatten() {
        let named = entry["summary"]
            .as_str()
            .is_some_and(|s| s.starts_with(PREFIX));
        if let (true, Some(id)) = (named, entry["id"].as_str()) {
            sharer
                .call(Method::DELETE, &format!("/calendars/{id}"), None)
                .await;
        }
    }
}

/// Creates one calendar holding one event and grants `role` on it to `grantee_email`.
async fn share_a_calendar(sharer: &Api, role: &str, grantee_email: &str) -> (String, String) {
    let (status, calendar) = sharer
        .call(
            Method::POST,
            "/calendars",
            Some(json!({ "summary": format!("{PREFIX}{role}") })),
        )
        .await;
    assert_eq!(status, 200, "create the {role} calendar: {calendar}");
    let calendar = calendar["id"].as_str().unwrap().to_owned();
    let event = json!({
        "summary": PROBE_EVENT,
        "start": { "dateTime": START },
        "end": { "dateTime": END },
    });
    let (status, event) = sharer
        .call(
            Method::POST,
            &format!("/calendars/{calendar}/events"),
            Some(event),
        )
        .await;
    assert_eq!(status, 200, "the probe event: {event}");
    let grant = json!({ "role": role, "scope": { "type": "user", "value": grantee_email } });
    let (status, granted) = sharer
        .call(
            Method::POST,
            &format!("/calendars/{calendar}/acl?sendNotifications=false"),
            Some(grant),
        )
        .await;
    assert_eq!(status, 200, "grant {role}: {granted}");
    (calendar, event["id"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn live_each_shared_role_grants_exactly_what_the_engine_says() {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(grantee_token), Some(sharer_token)) = (token(), var("GOOGLE_SHARER_ACCESS_TOKEN"))
    else {
        eprintln!(
            "skipping live_each_shared_role_grants_exactly_what_the_engine_says: set \
             GOOGLE_ACCESS_TOKEN and GOOGLE_SHARER_ACCESS_TOKEN (two accounts)"
        );
        return;
    };
    let grantee = Api::new(grantee_token.clone());
    let sharer = Api::new(sharer_token);
    let (_, primary) = grantee.call(Method::GET, "/calendars/primary", None).await;
    let grantee_email = primary["id"]
        .as_str()
        .expect("the grantee's address")
        .to_owned();

    delete_probe_calendars(&sharer).await;
    let mut probed = Vec::new();
    for role in ROLES {
        let (calendar, event) = share_a_calendar(&sharer, role, &grantee_email).await;
        // A calendar shared with an account is not in its list until it subscribes.
        let (status, entry) = grantee
            .call(
                Method::POST,
                "/users/me/calendarList",
                Some(json!({ "id": calendar })),
            )
            .await;
        assert!(matches!(status, 200 | 409), "subscribe to {role}: {entry}");
        probed.push((role, calendar, event));
    }

    // A subscription can take a moment to show in the list — one run read it back empty —
    // so wait until every grant reads as granted before judging the engine on it.
    let mut entries = Vec::new();
    for _ in 0..10 {
        let (_, list) = grantee
            .call(Method::GET, "/users/me/calendarList", None)
            .await;
        entries = probed
            .iter()
            .map(|(_, calendar, _)| {
                list["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|e| e["id"] == calendar.as_str())
                    .cloned()
                    .unwrap_or(Value::Null)
            })
            .collect();
        let settled = entries
            .iter()
            .zip(&probed)
            .all(|(entry, (role, ..))| entry["accessRole"] == *role);
        if settled {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // The engine's reading of each, through the adapter a host uses.
    let SyncUpdate::Snapshot { objects, .. } = calendar_provider(grantee_token)
        .sync_calendars(&account(), None)
        .await
        .expect("the grantee's calendar list")
        .update
    else {
        panic!("a calendar list is a snapshot");
    };
    let mut results = Vec::new();
    for ((role, calendar, event), entry) in probed.iter().zip(&entries) {
        // The captured shape, for `tests/fixtures/calendar/` (scrub the ids before committing).
        println!("calendarList entry for {role}: {entry}");
        let engine: Option<CalendarAccess> = objects
            .iter()
            .find(|c| c.id.as_str() == calendar)
            .map(|c| c.access);
        let observed = settled(&grantee, calendar, event, &grantee_email).await;
        results.push((*role, entry["accessRole"].clone(), engine, observed));
    }

    // Clean up before judging, so a failed assertion leaves nothing behind.
    for (_, calendar, _) in &probed {
        grantee
            .call(
                Method::DELETE,
                &format!("/users/me/calendarList/{calendar}"),
                None,
            )
            .await;
    }
    delete_probe_calendars(&sharer).await;

    for (role, reported, engine, observed) in results {
        println!("{role}: engine {engine:?}, server allowed {observed:?}");
        assert_eq!(reported, role, "the grant took effect as {role}");
        let access = engine.unwrap_or_else(|| panic!("{role}: the engine lists the calendar"));
        assert_eq!(access.may_read, observed.read, "{role}: read");
        assert_eq!(
            access.may_read_free_busy, observed.free_busy,
            "{role}: free/busy"
        );
        assert_eq!(access.may_write, observed.write, "{role}: write");
        assert_eq!(access.may_share, observed.share, "{role}: share");
        assert_eq!(access.may_delete, observed.delete, "{role}: delete");
    }
}
