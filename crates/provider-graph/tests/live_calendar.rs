//! Gated live integration for **Microsoft Graph calendars**: the calendar list, the
//! sync cycle, and the write path against a throwaway account.
//!
//! Split from the mail suite in `live_provider.rs` — same account and the same
//! `GRAPH_ACCESS_TOKEN` gate, different product surface.

use core::num::NonZeroU32;

use engine_core::{
    calendar::{Event, Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
    ids::{AccountId, CalendarId, Uid},
    membership::Memberships,
    sync::SyncUpdate,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget, Provider,
};
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Calendar (gated live)
// ---------------------------------------------------------------------------

fn calendar_window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

fn amsterdam() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// A calendar provider bound to `calendar`, reading times in Europe/Amsterdam.
fn calendar_provider(token: &str, calendar: CalendarId) -> GraphCalendarProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphCalendarProvider::new(client, calendar, calendar_window(), amsterdam())
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: amsterdam(),
    }
}

/// A minimal event carrying the identity + revision a write receipt reports, so a
/// follow-up patch/delete can guard on the ETag the create/patch returned.
fn base_from(
    receipt_event: &CalendarId,
    id: &str,
    uid: &Uid,
    revisions: engine_core::version::RevisionTokens,
) -> Event {
    let mut event = Event::new(
        engine_core::ids::EventId::try_from(id).unwrap(),
        uid.clone(),
        Memberships::of_one(receipt_event.clone()),
        zoned("2026-09-01T10:00:00"),
    );
    event.revisions = revisions;
    event
}

#[tokio::test]
async fn live_calendar_lists_syncs_and_writes() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_lists_syncs_and_writes: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    // List calendars and find the default (the binding used for events + writes).
    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let default = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar");
    let calendar_id = default.id.clone();

    let provider = calendar_provider(&token, calendar_id.clone());

    // A snapshot of the calendar's events: masters + singles, each zoned in the display
    // zone (proving the Prefer: outlook.timezone request), recurrence mapped for a series.
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    assert!(events.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    assert!(
        objects.iter().all(|e| matches!(
            e.start,
            CalendarDateTime::Zoned { .. } | CalendarDateTime::Date(_)
        )),
        "every event is zoned or all-day (never a bare UTC instant)"
    );
    // A delta from the fresh cursor is a delta, not a snapshot.
    let delta = provider
        .sync_events(&account(), Some(&events.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());

    // Create → patch → delete a throwaway event, guarding each write on the returned ETag.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-cal-{unique}@allodia-e2e.test")).unwrap();
    let draft = EventDraft::new(
        calendar_id.clone(),
        uid.clone(),
        "provider-graph live write probe",
        zoned("2026-09-15T10:00:00"),
        zoned("2026-09-15T10:30:00"),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    )
    .location("Room Z")
    .description("safe to delete");

    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create_event");
    assert!(
        created.revisions.etag.is_some(),
        "Graph returns an ETag on create"
    );

    // Rename it (a whole-series patch), guarded by the create's ETag.
    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap())
            .summary("live write probe (renamed)"),
    );
    let patched = provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch_event");
    assert!(patched.revisions.etag.is_some());
    assert_ne!(
        patched.revisions.etag, created.revisions.etag,
        "a patch advances the ETag"
    );

    // Delete it, guarded by the patch's ETag.
    let base = base_from(
        &calendar_id,
        patched.event.as_str(),
        &patched.uid,
        patched.revisions.clone(),
    );
    provider
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .expect("delete_event");
    // (A repeat delete of the just-deleted event is NOT retried here: Graph answers a
    // re-delete with `400 ErrorInvalidRequest` — the item has moved to Deleted Items — not
    // the clean `404` the idempotent path keys on. The 404 idempotency is offline-tested;
    // the outbox's NeedsConfirmation path covers the genuinely-ambiguous retry.)
}

/// Creating a **recurring** event, and reading the rule back off the server.
///
/// The offline suite can only prove the body we build; the fixture-routing fake answers
/// canned bytes whatever it is sent (`AGENTS.md`). Only a real create says whether Graph
/// accepts the `patternedRecurrence` this adapter renders — and only a real read-back says
/// whether what came home is the rule that went out.
#[tokio::test]
async fn live_calendar_creates_a_recurring_event() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_creates_a_recurring_event: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar_id = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone();
    let provider = calendar_provider(&token, calendar_id.clone());

    // Every Monday, eight times — the shape the product's repeat picker produces, and one
    // Graph states as `weekly` + `numbered` rather than as an RRULE.
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.by_day = vec![NDay {
        day: Weekday::Mo,
        nth_of_period: None,
    }];
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(8).unwrap());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-recur-{unique}@allodia-e2e.test")).unwrap();
    let draft = EventDraft::new(
        calendar_id.clone(),
        uid,
        "provider-graph live recurrence probe",
        zoned("2026-09-07T09:30:00"),
        zoned("2026-09-07T10:00:00"),
        "2026-08-23T10:00:00Z".parse().unwrap(),
    )
    .description("safe to delete")
    .repeating(DraftRecurrence::new(rule.clone()));

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
    assert_eq!(
        stored.recurrence.as_ref().unwrap().rules,
        vec![rule],
        "the rule Graph stored is the rule that was sent"
    );

    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    provider
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .expect("delete the probe series");
}
