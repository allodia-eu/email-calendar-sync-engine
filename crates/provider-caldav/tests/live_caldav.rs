//! Gated live integration: the **CalDAV calendar sync loop** against the Stalwart
//! harness.
//!
//! Drives `engine-sync` with the real `CalDavProvider` into a real `SqliteStore`,
//! then asserts the calendar seed *in the store*: the six fixtures normalize, the
//! recurring resource's master + `RECURRENCE-ID` override fold into one event with
//! an `EXDATE` exclusion, participants merge, the virtual location survives, and
//! every event materializes occurrences. A second sync proves the held sync-token
//! yields an idempotent empty delta. Skips with no `STALWART_HTTP_ADDR`, so the
//! offline `cargo test --workspace` stays green.
//!
//! Per the determinism rule, every assertion is on harness-controlled content
//! (iCalendar UIDs, titles, counts) — never on the server-assigned hrefs, ETags,
//! or sync-tokens.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, ProviderKey},
    sync::{SyncScope, SyncUpdate},
    time::TimeZoneId,
};
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use serde::de::DeserializeOwned;
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

mod common;
// Declared only here: the scheduling scenarios need a second principal and an
// auto-schedule server, which the SabreDAV fixture does not have (see the module docs).
mod scheduling;

async fn load<T: DeserializeOwned>(
    store: &SqliteStore<ManualClock>,
    scope: &SyncScope,
    key: &ProviderKey,
) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

#[tokio::test]
async fn caldav_calendar_sync_loop() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping caldav_calendar_sync_loop: STALWART_HTTP_ADDR unset");
        return;
    };
    // Serialize with the write round-trip: it transiently adds an event, which
    // would otherwise race this test's exact event-count assertion.
    let _serial = common::serial_guard().await;
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = CalDavProvider::connect(CalDavConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("connect + discover");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-live").unwrap();
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");
    assert!(
        report.calendars.upserted >= 1,
        "the default calendar synced"
    );

    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "six seed calendar resources stored");

    let mut events = Vec::new();
    for key in &event_keys {
        events.push(load::<Event>(&store, &event_scope, key).await);
    }
    let by_uid = |uid: &str| events.iter().find(|e| e.uid.as_str() == uid).unwrap();

    // The one-off zoned event, the meeting's three merged participants, the
    // virtual location, and the zoneless all-day event.
    assert_eq!(
        by_uid("oneoff-2001@test.local").title,
        "One-off zoned event"
    );
    assert_eq!(by_uid("meeting-2003@test.local").participants.len(), 3);
    assert_eq!(by_uid("virtual-2004@test.local").virtual_locations.len(), 1);
    assert!(by_uid("allday-2005@test.local").is_all_day());
    assert!(by_uid("floating-2006@test.local").start.is_floating());

    // The recurring resource folded master + override into one recurring event.
    let weekly = by_uid("weekly-2002@test.local");
    assert!(weekly.is_recurring());
    assert!(weekly.recurrence_id.is_none());

    // Occurrences materialized: weekly = 8 instances − 1 EXDATE = 7; 12 in total.
    let mut total = 0;
    for key in &event_keys {
        total += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    assert_eq!(
        store
            .index_row_counts(&event_scope, weekly.id.key())
            .await
            .unwrap()
            .occurrences,
        7
    );
    assert_eq!(total, 12);

    // A second sync reuses the held sync-token: an idempotent, empty delta.
    let second = sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("second sync_calendar");
    assert_eq!(
        second.events.applied.upserted, 0,
        "no event changes on a re-sync"
    );
    assert_eq!(
        second.events.applied.tombstoned, 0,
        "nothing tombstoned on a re-sync"
    );
    assert_eq!(
        store.object_keys(&event_scope).await.unwrap().len(),
        6,
        "the event set is unchanged after the delta"
    );
}

/// Connects a provider to the live Stalwart harness, or `None` to skip (offline gate).
async fn connect(test: &str) -> Option<(CalDavProvider, AccountId)> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let provider = CalDavProvider::connect(CalDavConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("connect + discover");
    Some((provider, AccountId::try_from("caldav-write-live").unwrap()))
}

/// The full CalDAV write lifecycle against the real Stalwart, driven off the `ETag`s the
/// `PUT`s return. Leaves the seed untouched. Skips with no `STALWART_HTTP_ADDR`.
#[tokio::test]
async fn caldav_write_round_trip() {
    let Some((provider, account)) = connect("caldav_write_round_trip").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::round_trip(&provider, &account).await;
}

/// The headline of #62: an edit made with the structural patcher (#58) survives the real
/// server. Stalwart **reserializes** what it stores — it re-folds content lines and
/// reorders `RRULE` parts — so this is the server that proves the preservation claim is
/// about content, not bytes on the wire.
#[tokio::test]
async fn caldav_patched_update_preserves_the_document() {
    let Some((provider, account)) = connect("caldav_patched_update_preserves_the_document").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::patched_update_preserves_the_document(&provider, &account).await;
}

/// A superseded `If-Match` really does come back `412` from Stalwart, and the adapter
/// classes it `Conflict` — the input the outbox needs to refetch-and-merge instead of
/// blindly retrying.
#[tokio::test]
async fn caldav_stale_if_match_is_a_conflict() {
    let Some((provider, account)) = connect("caldav_stale_if_match_is_a_conflict").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::stale_if_match_is_a_conflict(&provider, &account).await;
}

/// Stalwart accepts a resource carrying a master **and** a `RECURRENCE-ID` override the
/// patcher split out of it, and hands it back folded into one event.
#[tokio::test]
async fn caldav_instance_override_split_is_accepted() {
    let Some((provider, account)) = connect("caldav_instance_override_split_is_accepted").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::write::instance_override_split_is_accepted(&provider, &account).await;
}

/// Stalwart **does** advertise RFC 6638, so the discovered capability is `true` here — the
/// positive half of the pair whose negative is `sabredav_reports_no_scheduling` in
/// `live_sabredav.rs`. That the two disagree is what proves the flag is read off the
/// server rather than assumed from the protocol.
#[tokio::test]
async fn caldav_discovers_the_servers_scheduling_support() {
    let Some((provider, _account)) =
        connect("caldav_discovers_the_servers_scheduling_support").await
    else {
        return;
    };
    common::imip::scheduling_is_discovered_from_the_server(&provider, true);
}

/// Storing an invitation that arrived as mail, as a guarded create: Stalwart honours
/// `If-None-Match: *` on a `PUT`, and refuses the second one with a `412` rather than
/// overwriting the copy that is now there (issue #105).
#[tokio::test]
async fn caldav_storing_an_invitation_is_a_guarded_create() {
    let Some((provider, account)) =
        connect("caldav_storing_an_invitation_is_a_guarded_create").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::imip::storing_an_invitation_is_a_guarded_create(&provider, &account).await;
}

/// Stalwart reports `DAV:current-user-privilege-set` on Alice's own calendar, and it
/// grants `DAV:write` — so the collection the write tests above target reports itself
/// writable. The read-only half of this pair is SabreDAV's shared calendar
/// (`live_sabredav.rs`); Stalwart's harness account owns everything it can see, so it
/// cannot produce a collection it may not write.
#[tokio::test]
async fn caldav_reports_the_bound_calendar_as_writable() {
    let Some((provider, account)) = connect("caldav_reports_the_bound_calendar_as_writable").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;

    let calendars = provider
        .sync_calendars(&account, None)
        .await
        .expect("sync_calendars");
    let listed = match calendars.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    let bound = listed
        .iter()
        .find(|calendar| calendar.id.as_str() == provider.collection_href())
        .expect("the bound collection is listed");
    assert!(
        bound.access.may_write,
        "Stalwart grants DAV:write on the account's own calendar"
    );
    assert!(bound.access.may_read);
}

/// Read-your-writes (#65): a write reconciles the **store** to the server's copy, so a
/// host can re-read what it wrote — and guard its next edit on the revision the server
/// actually reported, instead of a `412` on the superseded one it wrote over.
#[tokio::test]
async fn caldav_write_reconciles_the_store() {
    let Some((provider, account)) = connect("caldav_write_reconciles_the_store").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::reconcile::read_your_writes(&provider, &account).await;
}

// ---------------------------------------------------------------------------
// Scheduling (RFC 6638 auto-schedule), Stalwart only — see `scheduling/mod.rs`.
//
// These share this binary's `serial_guard`, and they must: the server adds an invitation
// to Alice's own calendar, which would otherwise race the exact event-count assertion in
// `caldav_calendar_sync_loop` above.
// ---------------------------------------------------------------------------

/// An invitation the organizer stores lands on the attendee's calendar, owing a reply —
/// server-side scheduling, with the attendee's client having sent nothing.
#[tokio::test]
async fn caldav_invitation_is_delivered_to_the_attendee() {
    let Some(parties) = scheduling::parties("caldav_invitation_is_delivered_to_the_attendee").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_invitation_is_delivered_to_the_attendee(&parties).await;
}

/// An Outlook-shaped `TZID=W. Europe Standard Time` invitation resolves to `Europe/Berlin`
/// on the attendee's copy — including the DQUOTE-quoting the server adds.
#[tokio::test]
async fn caldav_invitation_windows_time_zone_resolves_to_iana() {
    let Some(parties) =
        scheduling::parties("caldav_invitation_windows_time_zone_resolves_to_iana").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_invitations_windows_time_zone_resolves_to_iana(&parties).await;
}

/// A real server's `METHOD:REQUEST`, taken from the RFC 6638 scheduling inbox, parses
/// through the engine's one iCalendar parser.
#[tokio::test]
async fn caldav_scheduling_inbox_carries_a_parseable_itip_request() {
    let Some(parties) =
        scheduling::parties("caldav_scheduling_inbox_carries_a_parseable_itip_request").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::the_scheduling_inbox_carries_a_parseable_itip_request(&parties).await;
}

/// The headline: patching my `PARTSTAT` and `PUT`ting it back is the whole RSVP — the
/// organizer's own copy shows the acceptance, with no client-side iTIP delivery.
#[tokio::test]
async fn caldav_rsvp_reaches_the_organizer() {
    let Some(parties) = scheduling::parties("caldav_rsvp_reaches_the_organizer").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_rsvp_reaches_the_organizer(&parties).await;
}

/// The same round trip through `Provider::rsvp_event` — the verb a host actually calls —
/// plus the two controls CalDAV must refuse rather than silently drop.
#[tokio::test]
async fn caldav_rsvp_through_the_neutral_verb_reaches_the_organizer() {
    let Some(parties) =
        scheduling::parties("caldav_rsvp_through_the_neutral_verb_reaches_the_organizer").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_rsvp_through_the_neutral_verb_reaches_the_organizer(&parties).await;
}

/// The RSVP receipt reports exactly what the server said about delivering the reply — which
/// on Stalwart is nothing at all, even though the reply demonstrably arrives.
#[tokio::test]
async fn caldav_rsvp_receipt_reports_what_the_server_said_about_delivery() {
    let Some(parties) =
        scheduling::parties("caldav_rsvp_receipt_reports_what_the_server_said_about_delivery")
            .await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_rsvp_receipt_reports_what_the_server_said_about_delivery(&parties).await;
}

/// An organizer's delete reaches the attendee as `STATUS:CANCELLED` — the attendee's copy
/// is tombstoned, not removed.
#[tokio::test]
async fn caldav_organizer_cancel_marks_the_attendees_copy_cancelled() {
    let Some(parties) =
        scheduling::parties("caldav_organizer_cancel_marks_the_attendees_copy_cancelled").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::an_organizer_cancel_marks_the_attendees_copy_cancelled(&parties).await;
}

/// The fixture polices itself: a scheduling run leaves no `.ics` on either calendar and no
/// message in the scheduling inbox, so a long-lived harness does not accumulate residue.
#[tokio::test]
async fn caldav_scheduling_cleanup_leaves_no_residue() {
    let Some(parties) = scheduling::parties("caldav_scheduling_cleanup_leaves_no_residue").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    scheduling::cleanup_leaves_no_residue(&parties).await;
}

/// A recurring create really lands on Stalwart, and the rule reads back through the
/// server's own reserialization.
#[tokio::test]
async fn caldav_create_carries_the_recurrence_rule() {
    let Some((provider, account)) = connect("caldav_create_carries_the_recurrence_rule").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::create_carries_the_rule(&provider, &account).await;
}

/// A series ending at a wall clock reaches the server with `UNTIL` in UTC, and the draft
/// that omits the resolved instant is refused rather than written with a local clock.
#[tokio::test]
async fn caldav_recurrence_until_is_written_in_utc() {
    let Some((provider, account)) = connect("caldav_recurrence_until_is_written_in_utc").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::an_until_is_written_in_utc(&provider, &account).await;
}

/// Changing a rule keeps the per-occurrence work; removing one takes it with it.
#[tokio::test]
async fn caldav_a_rule_can_be_changed_and_removed() {
    let Some((provider, account)) = connect("caldav_a_rule_can_be_changed_and_removed").await
    else {
        return;
    };
    let _serial = common::serial_guard().await;
    common::recurrence::a_rule_can_be_changed_and_removed(&provider, &account).await;
}
