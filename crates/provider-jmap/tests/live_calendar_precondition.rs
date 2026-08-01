//! Gated live proof of *why* this adapter sends no `ifInState`, on the one question no test
//! that drives the adapter can reach. Skips with no `STALWART_HTTP_ADDR`.
//!
//! # The gap this fills
//!
//! `provider-jmap` advertises [`WriteGuard::Absent`] for calendar writes. The sibling
//! `a_stale_edit_is_not_refused` pins that from the adapter side, and is explicit about its
//! blind spot: it sends no precondition, so it cannot observe what the server does with one.
//! Nothing in the repo could, because the adapter has no way to express `ifInState` — by
//! design. That left the *reason* for the design sitting in prose, which is how a claim about
//! it went unverified across two releases (issue #93).
//!
//! This file closes that, over the harness's raw JMAP seam (`Harness::jmap_post`).
//!
//! # What it proves, and why it argues *against* sending `ifInState`
//!
//! Stalwart v0.16.14+ (the harness pins **v0.16.15**) enforces `ifInState` correctly: a
//! superseded token is refused with `stateMismatch` and the write does not land. So the
//! objection is **not** "the server ignores it" — it honours it exactly as RFC 8620 §5.3
//! specifies, and that is the problem.
//!
//! §5.3 scopes the token to the account's whole `CalendarEvent` **type state**, not to the
//! object being written. [`ifinstate_refuses_a_write_because_an_unrelated_event_changed`]
//! demonstrates the consequence against the real server: hold a state, let a *different*
//! property of a *different* event change, and a write to an event **nobody touched** is
//! refused. For a sync engine holding a cursor while the account keeps moving underneath it —
//! an incoming invitation, another device, a series materializing — that is not lost-update
//! protection, it is a write that fails on unrelated activity.
//!
//! So `ifInState` is the wrong *instrument*, independent of how well a server implements it,
//! and there is no right one here: a `CalendarEvent` carries no per-object revision (no
//! `ETag`, no `changeKey`), so `RevisionTokens` is empty for every JMAP object by
//! construction. There is nothing to name *this event's* version with. `jmap.md` is
//! authoritative; this is its executable half.
//!
//! **If this test starts failing**, the server stopped enforcing or changed the scope — read
//! `jmap.md`'s "no per-object revision" section before concluding anything about the adapter.

use serde_json::{Value, json};
use stalwart_harness::Harness;

/// The event this probe writes to. Its own, so a failed run cannot disturb the seed.
const MINE_UID: &str = "jmap-precondition-mine@test.local";
/// The event this probe edits to move the account's shared `CalendarEvent` state.
const OTHER_UID: &str = "jmap-precondition-other@test.local";

/// The seeded account's `CalendarEvent` account id, from the JMAP session.
fn calendar_account(harness: &Harness) -> String {
    harness.jmap_session().expect("jmap session")["primaryAccounts"]
        ["urn:ietf:params:jmap:calendars"]
        .as_str()
        .expect("a primary calendar account")
        .to_owned()
}

/// Sends one `methodCalls` array and returns the parsed response envelope.
fn call(harness: &Harness, method_calls: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:calendars"],
        "methodCalls": method_calls,
    });
    let resp = harness
        .jmap_post(body.to_string().as_bytes())
        .expect("POST /jmap/");
    serde_json::from_slice(&resp.body).unwrap_or_else(|error| {
        panic!("JMAP response was not JSON ({error}): {}", resp.body_text())
    })
}

/// The first method response's name and arguments.
fn first(response: &Value) -> (&str, &Value) {
    (
        response["methodResponses"][0][0]
            .as_str()
            .unwrap_or_default(),
        &response["methodResponses"][0][1],
    )
}

/// Renames `id`, unguarded, and returns the `newState` the server reports.
fn rename(harness: &Harness, account: &str, id: &str, title: &str) -> String {
    let response = call(
        harness,
        &json!([[
            "CalendarEvent/set",
            {"accountId": account, "update": {id: {"title": title}}},
            "s"
        ]]),
    );
    let (name, set) = first(&response);
    assert_eq!(name, "CalendarEvent/set", "unguarded rename: {response}");
    assert!(
        set["updated"].get(id).is_some(),
        "the unguarded rename must land, or the probe proves nothing: {set}"
    );
    set["newState"]
        .as_str()
        .expect("a newState on every /set")
        .to_owned()
}

/// The event's current title, as the server holds it.
fn title_of(harness: &Harness, account: &str, id: &str) -> String {
    let response = call(
        harness,
        &json!([[
            "CalendarEvent/get",
            {"accountId": account, "ids": [id], "properties": ["title"]},
            "g"
        ]]),
    );
    response["methodResponses"][0][1]["list"][0]["title"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The account's first calendar id — where this probe's throwaway events land.
fn calendar_id(harness: &Harness, account: &str) -> String {
    let response = call(
        harness,
        &json!([["Calendar/query", {"accountId": account}, "q"]]),
    );
    response["methodResponses"][0][1]["ids"][0]
        .as_str()
        .expect("the seeded account has a calendar")
        .to_owned()
}

/// The JMAP id of the event with `uid`, if the server holds one.
fn find_by_uid(harness: &Harness, account: &str, uid: &str) -> Option<String> {
    let response = call(
        harness,
        &json!([
            ["CalendarEvent/query", {"accountId": account}, "q"],
            ["CalendarEvent/get", {
                "accountId": account,
                "#ids": {"resultOf": "q", "name": "CalendarEvent/query", "path": "/ids"},
                "properties": ["id", "uid"],
            }, "g"],
        ]),
    );
    response["methodResponses"][1][1]["list"]
        .as_array()?
        .iter()
        .find(|event| event["uid"].as_str() == Some(uid))
        .map(|event| event["id"].as_str().expect("an id").to_owned())
}

/// Destroys the event with `uid` if it exists — this probe's own residue, never the seed.
fn destroy_by_uid(harness: &Harness, account: &str, uid: &str) {
    if let Some(id) = find_by_uid(harness, account, uid) {
        call(
            harness,
            &json!([[
                "CalendarEvent/set",
                {"accountId": account, "destroy": [id]},
                "s"
            ]]),
        );
    }
}

/// Creates a throwaway event carrying `uid`, replacing any residue from an interrupted run,
/// and returns the id the server assigned.
///
/// **This probe never touches the seeded events.** It rewrites titles and deliberately
/// provokes a failed write, and an earlier draft that did this to the seed left
/// `caldav_one_off_event_present` red when it panicked before its restore step. Owning the
/// data removes the failure mode rather than tidying up after it.
fn create_throwaway(harness: &Harness, account: &str, calendar: &str, uid: &str) -> String {
    destroy_by_uid(harness, account, uid);
    let response = call(
        harness,
        &json!([[
            "CalendarEvent/set",
            {
                "accountId": account,
                "create": {"n": {
                    "@type": "Event",
                    "calendarIds": {calendar: true},
                    "uid": uid,
                    "title": "precondition probe: created",
                    "start": "2026-09-01T10:00:00",
                    "timeZone": "Europe/Amsterdam",
                    "duration": "PT1H",
                }},
            },
            "s"
        ]]),
    );
    let (name, set) = first(&response);
    assert_eq!(name, "CalendarEvent/set", "create a throwaway: {response}");
    set["created"]["n"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the server assigns the id: {set}"))
        .to_owned()
}

/// A write guarded by `ifInState` is refused because an **unrelated** event changed — the
/// spurious rejection that makes an account-scoped token the wrong per-event guard.
///
/// The sequence is the one a sync engine actually lives in: it holds a state from its last
/// sync, the account moves for reasons that have nothing to do with the user's edit, and the
/// edit is then rejected with `stateMismatch` even though *its* event was never touched.
///
/// Note the failure is a **top-level method error**, not a per-object `notUpdated` entry, so a
/// caller cannot even narrow it to the object it wrote: one unrelated change loses every write
/// in a batched `/set`.
///
/// Verified to fail for the right reason: drop the `ifInState` argument and the write lands —
/// which is exactly what the adapter sends, and why it is unaffected.
#[tokio::test]
async fn ifinstate_refuses_a_write_because_an_unrelated_event_changed() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping ifinstate_refuses_a_write_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(core::time::Duration::from_secs(30))
        .expect("harness ready");
    let account = calendar_account(&harness);
    let calendar = calendar_id(&harness, &account);

    // Two events this probe owns outright: one it writes to, one it uses to move the shared
    // type state. Nothing here touches the seed the other suites assert on.
    let mine = create_throwaway(&harness, &account, &calendar, MINE_UID);
    let other = create_throwaway(&harness, &account, &calendar, OTHER_UID);
    let (mine, other) = (&mine, &other);

    // 1. The state a synced client legitimately holds after its own last write.
    let held = rename(&harness, &account, mine, "precondition probe: baseline");

    // 2. A DIFFERENT property of a DIFFERENT event changes — the account moving on its own.
    let after_unrelated = rename(&harness, &account, other, "precondition probe: unrelated");
    assert_ne!(
        held, after_unrelated,
        "an edit to another event must advance the shared type state — that sharing is the \
         whole reason `ifInState` cannot be a per-event guard"
    );

    // 3. Our write, to an event nobody else touched, guarded by the state we held.
    let guarded = call(
        &harness,
        &json!([[
            "CalendarEvent/set",
            {
                "accountId": account,
                "ifInState": held,
                "update": {mine: {"title": "precondition probe: guarded by a stale token"}},
            },
            "s"
        ]]),
    );
    let (name, error) = first(&guarded);
    assert_eq!(
        name, "error",
        "expected the whole method call to fail, not a per-object rejection: {guarded}"
    );
    assert_eq!(
        error["type"].as_str(),
        Some("stateMismatch"),
        "the RFC 8620 §5.3 refusal is what we want the server to do — and precisely why \
         sending the token would break writes on unrelated activity: {guarded}"
    );
    assert_eq!(
        title_of(&harness, &account, mine),
        "precondition probe: baseline",
        "the refused write must not have landed, or `stateMismatch` would be advisory only"
    );

    // 4. The same write without the guard — what the adapter sends — lands.
    rename(
        &harness,
        &account,
        mine,
        "precondition probe: unguarded lands",
    );
    assert_eq!(
        title_of(&harness, &account, mine),
        "precondition probe: unguarded lands",
        "the adapter's own write is unaffected by the account having moved"
    );

    // Take our two events away again. A panic above skips this, which is why the probe owns
    // them: the residue is two events with our own UIDs, which the next run replaces.
    for uid in [MINE_UID, OTHER_UID] {
        destroy_by_uid(&harness, &account, uid);
        assert!(
            find_by_uid(&harness, &account, uid).is_none(),
            "the probe must take its own events away: {uid}"
        );
    }
}
