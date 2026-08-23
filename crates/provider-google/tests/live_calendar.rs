//! Gated live integration for **Google Calendar**: the calendar list, the snapshot/delta
//! cycle, and the create/patch/delete write path against a throwaway account.
//!
//! Split from the Gmail suite in `live_provider.rs` — same account and the same
//! `GOOGLE_ACCESS_TOKEN` gate, different product surface.

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::SyncUpdate,
};
use engine_provider::Provider;
use provider_google::{GoogleCalendarProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

// --- Google Calendar (Phase D) ---

fn calendar_provider(token: String) -> GoogleCalendarProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    // "primary" is Google's alias for the account's default calendar.
    GoogleCalendarProvider::new(client, CalendarId::try_from("primary").unwrap())
}

#[tokio::test]
async fn live_calendars_list() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendars_list: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let sync = calendar_provider(token)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a calendar snapshot");
    };
    // The account has at least its own primary calendar.
    assert!(objects.iter().any(|c| c.is_default), "a primary calendar");
}

#[tokio::test]
async fn live_calendar_snapshot_then_delta_cycle() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_snapshot_then_delta_cycle: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    // A first sync is a reconciling snapshot; capture the per-calendar syncToken.
    let snapshot = provider
        .sync_events(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let cursor = snapshot.next_cursor.clone();
    assert!(!cursor.as_str().is_empty());

    // An immediate delta from that syncToken must not error and must not be a snapshot —
    // proving the real events.list?syncToken request shape is accepted.
    let delta = provider
        .sync_events(&account(), Some(&cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot(), "a delta from a live syncToken");
    assert!(!delta.next_cursor.as_str().is_empty());
}

#[tokio::test]
async fn live_calendar_create_patch_delete() {
    use engine_core::{
        calendar::Event,
        ids::Uid,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    };
    use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_create_patch_delete: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-07-18T10:00:00Z".parse().unwrap();
    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };

    // Create a throwaway event.
    let draft = EventDraft::new(
        cal.clone(),
        Uid::new(format!("live-cal-{}@example.test", std::process::id())).unwrap(),
        "Live create/patch/delete",
        zoned("2026-09-01T10:00:00"),
        zoned("2026-09-01T10:30:00"),
        stamp,
    )
    .location("Room Live");
    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create");
    let first_etag = created.revisions.etag.clone();
    assert!(first_etag.is_some(), "the created event carries an ETag");

    // Build the base event as read, then patch it (rename) — the ETag must advance.
    let mut base = Event::new(
        created.event.clone(),
        created.uid.clone(),
        engine_core::membership::Memberships::of_one(cal.clone()),
        zoned("2026-09-01T10:00:00"),
    );
    base.revisions = created.revisions.clone();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp).summary("Live create/patch/delete (renamed)"),
    );
    let patched = provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch");
    assert!(
        patched.revisions.etag.is_some() && patched.revisions.etag != first_etag,
        "the ETag advances on patch"
    );

    // Delete it, guarded by the fresh ETag.
    base.revisions = patched.revisions.clone();
    provider
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .expect("delete");
    // NOTE: a *guarded* re-delete here returns 412 (conditionNotMet), not 404/410 — the
    // deleted event is left cancelled with a new ETag, so the stale If-Match fails the
    // precondition (a real Google finding; see tests/fixtures/README.md). The
    // 404/410-gone idempotency is covered offline (`cal_write` tests).
}

/// Creating a **recurring** event, and reading the rule back off the server.
///
/// The offline suite proves only the body we build — the fake answers canned bytes
/// whatever it is sent (`AGENTS.md`). Only a real create says whether Google accepts the
/// `RRULE` line this adapter renders, and only a real read-back says whether the rule that
/// came home is the rule that went out.
#[tokio::test]
async fn live_calendar_creates_a_recurring_event() {
    use engine_core::{
        calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
        ids::Uid,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    };
    use engine_provider::{DraftRecurrence, EventDeletion, EventDraft};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_creates_a_recurring_event: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();
    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };

    // Every Monday until 26 October — the "ends on this day" shape the product's repeat
    // picker produces, and the one that obliges `UNTIL` in UTC (RFC 5545 §3.3.10). The
    // resolved instant is stated because no adapter carries tzdata: 23:59:59 in
    // Europe/Amsterdam is 22:59:59Z, CEST being UTC+2 on that date.
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());

    let draft = EventDraft::new(
        cal.clone(),
        Uid::new(format!("live-recur-{}@example.test", std::process::id())).unwrap(),
        "Live recurrence probe",
        zoned("2026-09-07T09:30:00"),
        zoned("2026-09-07T10:00:00"),
        stamp,
    )
    .repeating(DraftRecurrence::ending_at(
        rule.clone(),
        "2026-10-26T22:59:59Z".parse().unwrap(),
    ));

    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create a recurring event");

    // Read it back through the adapter's own sync path: what a host would see.
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    let stored = objects
        .iter()
        .find(|e| e.id == created.event)
        .expect("the created series is in the snapshot");

    assert!(
        stored.is_recurring(),
        "the created event came back as a series master"
    );
    let stored_rule = &stored.recurrence.as_ref().unwrap().rules[0];
    assert_eq!(stored_rule.frequency, rule.frequency);
    assert_eq!(stored_rule.by_day, rule.by_day);
    // Google echoes the UNTIL back as the UTC instant it was sent, which the shared parser
    // reads as that instant's own wall clock — so the round trip lands on 22:59:59, not on
    // the 23:59:59 Amsterdam clock it was authored from. Pinning it keeps the asymmetry
    // visible: a host that wants the authored clock re-resolves through the event's zone.
    assert_eq!(
        stored_rule.bound,
        RecurrenceBound::Until("2026-10-26T22:59:59".parse().unwrap()),
    );

    let mut base = engine_core::calendar::Event::new(
        created.event.clone(),
        created.uid.clone(),
        engine_core::membership::Memberships::of_one(cal),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();
    provider
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
