//! `CalDavProvider` tests: scope/capability wiring and the **full offline sync
//! loop** — the real provider (with a fake executor replaying captured Stalwart
//! transcripts) driven through `engine_sync::sync_calendar` into a real
//! `SqliteStore`, asserting the seed normalizes, the master+override folds, and
//! occurrences materialize. This is the CalDAV analogue of `provider-jmap`'s
//! `live_sync`, but deterministic and Docker-free.

use core::time::Duration;

use engine_core::calendar::{Calendar, Event};
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::SyncScope;
use engine_core::time::TimeZoneId;
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::sync_calendar;
use serde::de::DeserializeOwned;
use store_sqlite::SqliteStore;

use super::*;
use crate::test_support::Replay;

fn replay(bodies: Vec<&str>) -> Replay {
    Replay::bodies(bodies)
}

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");
const HOME: &str = include_str!("../tests/fixtures/calendar-home.xml");
const SYNC_INITIAL: &str = include_str!("../tests/fixtures/sync-initial.xml");

async fn connect(exec: Replay) -> CalDavProvider {
    CalDavProvider::with_executor(Box::new(exec), "/.well-known/caldav", "default")
        .await
        .expect("discovery")
}

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

#[test]
fn resolves_relative_and_absolute_collections() {
    assert_eq!(
        resolve_collection("/dav/cal/u/", "default"),
        "/dav/cal/u/default/"
    );
    assert_eq!(resolve_collection("/dav/cal/u", "work"), "/dav/cal/u/work/");
    // An absolute collection path is used verbatim (with a trailing slash).
    assert_eq!(
        resolve_collection("/dav/cal/u/", "/shared/team/"),
        "/shared/team/"
    );
    // A full-URL href (as some servers return) passes through unchanged.
    assert_eq!(
        resolve_collection("/dav/cal/u/", "https://dav.example.com/cal/x/"),
        "https://dav.example.com/cal/x/"
    );
}

#[tokio::test]
async fn exposes_dav_scopes_and_the_calendar_capabilities() {
    let provider = connect(replay(vec![PRINCIPAL])).await;
    let account = AccountId::try_from("a").unwrap();

    // CalDAV does calendar read/sync **and** writes over the same HTTP transport;
    // it does no mail.
    assert!(provider.capabilities().calendars());
    assert!(provider.capabilities().calendar_writes());
    assert!(!provider.capabilities().mail());
    assert!(!provider.capabilities().submission());

    assert_eq!(
        provider.calendar_scope(&account),
        SyncScope::DavCollectionList {
            account: account.clone()
        }
    );
    match provider.event_scope(&account) {
        SyncScope::DavCollection { collection, .. } => {
            assert_eq!(collection.as_str(), "/dav/cal/alice%40test.local/default/");
        }
        other => panic!("expected a DavCollection scope, got {other:?}"),
    }
    assert_eq!(
        provider.collection_href(),
        "/dav/cal/alice%40test.local/default/"
    );
}

// One cohesive end-to-end flow (discover → list calendars → sync events → assert
// normalization + occurrences); splitting it would obscure the single scenario.
#[tokio::test]
async fn calendar_sync_loop_normalizes_folds_and_expands_the_seed() {
    let provider = connect(replay(vec![PRINCIPAL, HOME, SYNC_INITIAL])).await;
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("t"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");

    // ---- Calendars: the one default collection, applied as a container. ----
    let calendar_scope = provider.calendar_scope(&account);
    let calendar_keys = store.object_keys(&calendar_scope).await.unwrap();
    assert_eq!(calendar_keys.len(), 1);
    let calendar: Calendar = load(&store, &calendar_scope, &calendar_keys[0]).await;
    assert_eq!(calendar.name, "Stalwart Calendar (alice@test.local)");

    // ---- Events: six seed resources, each a member of the bound calendar. ----
    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "six seed resources stored");

    let mut events = Vec::new();
    for key in &event_keys {
        events.push(load::<Event>(&store, &event_scope, key).await);
    }
    assert!(
        events.iter().all(|e| e.calendars.contains(&calendar.id)),
        "every event references the bound calendar (referential integrity)"
    );

    // The meeting normalized its merged participants and the virtual location.
    let meeting = events
        .iter()
        .find(|e| e.uid.as_str() == "meeting-2003@test.local")
        .unwrap();
    assert_eq!(meeting.participants.len(), 3);
    let virtual_event = events
        .iter()
        .find(|e| e.uid.as_str() == "virtual-2004@test.local")
        .unwrap();
    assert_eq!(virtual_event.virtual_locations.len(), 1);

    // The recurring resource folded its master + RECURRENCE-ID override into one
    // recurring event whose raw iCalendar is preserved.
    let weekly = events
        .iter()
        .find(|e| e.uid.as_str() == "weekly-2002@test.local")
        .unwrap();
    assert!(weekly.is_recurring());
    assert!(weekly.recurrence_id.is_none());
    assert!(
        weekly
            .raw_ical
            .as_ref()
            .unwrap()
            .as_str()
            .contains("RECURRENCE-ID")
    );

    // ---- Occurrences: every event materializes, recurrence honored end to end. ----
    let mut total = 0;
    for key in &event_keys {
        total += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    let weekly_occurrences = store
        .index_row_counts(&event_scope, weekly.id.key())
        .await
        .unwrap()
        .occurrences;
    // 8 weekly instances − 1 EXDATE = 7 (the moved RECURRENCE-ID instance is still
    // one occurrence, just at its overridden time).
    assert_eq!(weekly_occurrences, 7);
    // oneoff(1) + weekly(7) + meeting(1) + virtual(1) + all-day(1) + floating(1).
    assert_eq!(total, 12);
}

#[tokio::test]
async fn calendar_list_includes_a_bound_collection_outside_the_home() {
    // A provider bound to an absolute collection NOT under the discovered home:
    // sync_calendars must still represent it, so events synced under it never
    // reference a calendar the container snapshot omits.
    // PRINCIPAL drives discovery; HOME is the calendar-list response (it lists only
    // the default collection, NOT /shared/team-calendar/).
    let provider = CalDavProvider::with_executor(
        Box::new(replay(vec![PRINCIPAL, HOME])),
        "/.well-known/caldav",
        "/shared/team-calendar/",
    )
    .await
    .expect("discovery");
    let account = AccountId::try_from("acct").unwrap();

    let listed = provider
        .sync_calendars(&account, None)
        .await
        .expect("sync_calendars");
    let objects = match &listed.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { .. } => panic!("calendar list is a snapshot"),
    };
    assert!(
        objects
            .iter()
            .any(|c| c.id.as_str() == "/shared/team-calendar/"),
        "the bound out-of-home collection is represented in the container snapshot"
    );
    // The list cursor is the named sentinel, never the empty string.
    assert_eq!(listed.next_cursor.as_str(), "caldav-calendar-list");
}

#[tokio::test]
async fn rebind_switches_collection_without_rediscovery() {
    // connect runs discovery once; rebind reuses the home + executor with no extra
    // PROPFIND, only moving the bound collection.
    let provider = connect(replay(vec![PRINCIPAL])).await;
    let account = AccountId::try_from("acct").unwrap();
    let rebound = provider.rebind("/calendars/other/").expect("rebind");
    match rebound.event_scope(&account) {
        SyncScope::DavCollection { collection, .. } => {
            assert_eq!(collection.as_str(), "/calendars/other/");
        }
        other => panic!("expected a DavCollection scope, got {other:?}"),
    }
    assert_eq!(rebound.collection_href(), "/calendars/other/");
}

#[tokio::test]
async fn mints_a_resource_href_under_the_bound_collection() {
    let provider = connect(replay(vec![PRINCIPAL])).await;
    // The conventional `<collection>/<uid>.ics`, with the UID canonically encoded:
    // `@` → `%40` (the form servers store and report — verified live against
    // Stalwart), so the minted href matches the server's resource href for a later
    // If-Match/DELETE.
    let href = provider
        .event_href(&Uid::new("oneoff-2001@test.local").unwrap())
        .unwrap();
    assert_eq!(
        href.as_str(),
        "/dav/cal/alice%40test.local/default/oneoff-2001%40test.local.ics"
    );
    // A path-unsafe UID (space, slash) is percent-encoded into one segment, so the
    // href stays a single valid resource name.
    let odd = provider.event_href(&Uid::new("a b/c").unwrap()).unwrap();
    assert_eq!(
        odd.as_str(),
        "/dav/cal/alice%40test.local/default/a%20b%2Fc.ics"
    );
}

// The model invariant (`calendar-semantics.md`, `modeling.md`): a CalDAV event
// carrying properties absent from JSCalendar round-trips via **raw-plus-patch**
// without dropping them. We parse a resource whose body carries an `X-` property
// and a `VALARM` the lossy projection cannot express, then PUT its preserved
// `raw_ical` — the wire body must still carry both, proving the write round-trips
// from raw, never from a re-serialized projection.
#[tokio::test]
async fn an_update_round_trips_raw_ical_preserving_non_jscalendar_properties() {
    use crate::ical::parse_calendar_object;
    use crate::test_support::wrote;
    use engine_core::ids::{CalendarId, EventId};
    use engine_core::version::ETag;
    use engine_provider::EventWrite;

    let resource = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
        UID:rt-9001@test.local\r\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\r\n\
        DTEND;TZID=Europe/Amsterdam:20260318T110000\r\nSUMMARY:Round trip\r\n\
        X-CUSTOM-FLAG:keep-me\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\n\
        DESCRIPTION:Reminder\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let href = EventId::try_from("/dav/cal/alice%40test.local/default/rt-9001.ics").unwrap();
    let parsed = parse_calendar_object(
        resource,
        href.clone(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("parse");
    // The lossy projection has no typed field for the X- property, but raw_ical kept it.
    let raw = parsed.raw_ical.clone().expect("raw preserved");

    // A shared executor handle, so the test can inspect the wire body after the
    // provider (which owns its executor) performs the PUT. Discovery consumes
    // PRINCIPAL, then the PUT consumes the write response.
    let exec = std::sync::Arc::new(Replay::new(vec![
        crate::test_support::ok(PRINCIPAL),
        wrote(201, Some("\"rt-v2\"")),
    ]));
    let provider =
        CalDavProvider::with_executor(Box::new(exec.clone()), "/.well-known/caldav", "default")
            .await
            .expect("discovery");
    let account = AccountId::try_from("acct").unwrap();
    let write = EventWrite::update(
        href.clone(),
        parsed.uid.clone(),
        raw,
        ETag::new("\"rt-v1\""),
    );
    let receipt = provider.put_event(&account, &write).await.expect("put");

    assert_eq!(receipt.event_key, href.key().clone());
    assert_eq!(receipt.etag, Some(ETag::new("\"rt-v2\"")));

    // The PUT body still carries the X- property and the VALARM — nothing the
    // projection cannot express was dropped.
    let writes = exec.writes();
    assert_eq!(writes[0].method, crate::transport::DavMethod::Put);
    assert!(writes[0].body.contains("X-CUSTOM-FLAG:keep-me"));
    assert!(writes[0].body.contains("BEGIN:VALARM"));
    assert!(writes[0].body.contains("TRIGGER:-PT15M"));
}

// --- iTIP/iMIP end-to-end: parse → reconcile → trust → RSVP → outbox → store ---

/// A stored (no transit-only METHOD, RFC 4791 §4.1) copy of the invited event, as
/// my calendar holds it after a CalDAV auto-schedule server processed the REQUEST:
/// I am a needs-action attendee.
const STORED_INVITE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// The inbound iMIP REQUEST that delivered the invite (the same event, carrying a
/// `METHOD`), as parsed off the mail path.
const INVITE_REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nSEQUENCE:0\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

// One cohesive scenario (parse the invite → trust it against its organizer →
// accept → write my PARTSTAT back through the conditional-PUT outbox driver into a
// real store); splitting it would obscure the single inbound→outbound flow.
#[tokio::test]
async fn an_accepted_invite_rsvps_via_a_conditional_put_through_the_outbox() {
    use crate::ical::parse_calendar_object;
    use crate::imip;
    use crate::test_support::{ok, wrote};
    use crate::transport::{DavMethod, Precondition};
    use engine_core::calendar::ParticipationStatus;
    use engine_core::ids::CalendarId;
    use engine_core::raw::RawIcal;
    use engine_core::scheduling::{ScheduleAction, reconcile};
    use engine_core::version::ETag;
    use engine_provider::EventWrite;
    use engine_sync::write_calendar_event;

    // (1) Parse the inbound iMIP REQUEST off the mail path.
    let message = imip::parse(INVITE_REQUEST).expect("parse imip request");
    let uid = message.event.uid.clone();
    let me = "me@test.local";

    // (2) Trust + reconcile: the authenticated sender IS the organizer, and the
    // instance is unseen, so the decision is to schedule the event.
    assert_eq!(
        reconcile(&message, Some("boss@test.local"), None),
        ScheduleAction::ScheduleEvent
    );

    // (3) I accept: patch *my* PARTSTAT into my stored copy of the event (the RSVP
    // write primitive). Storage round-trips from raw plus this targeted patch.
    let patched = imip::set_my_partstat(
        &RawIcal::new(STORED_INVITE),
        me,
        &ParticipationStatus::Accepted,
    )
    .expect("rsvp patch");

    // (4) Drive the conditional PUT through the existing outbox driver into a real
    // store. Discovery consumes PRINCIPAL; the PUT consumes the write response.
    let exec = std::sync::Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        wrote(204, Some("\"rt-v2\"")),
    ]));
    let provider =
        CalDavProvider::with_executor(Box::new(exec.clone()), "/.well-known/caldav", "default")
            .await
            .expect("discovery");
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-20T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("caldav-acct").unwrap();
    let href = provider.event_href(&uid).expect("href");
    let write = EventWrite::update(href.clone(), uid, patched, ETag::new("\"rt-v1\""));

    let outcome = write_calendar_event(
        &provider,
        &store,
        &account,
        WorkerId::new("t"),
        Duration::from_mins(5),
        "rsvp:meeting-7@test.local:accept",
        &write,
    )
    .await
    .expect("rsvp write");

    // (5) The op succeeded and recorded the server's new ETag.
    assert_eq!(outcome.event_key, href.key().clone());
    assert_eq!(outcome.etag, Some(ETag::new("\"rt-v2\"")));

    // The PUT was guarded by If-Match (optimistic concurrency, never a blind
    // overwrite), carried no transit-only METHOD, and its body sets my accepted
    // status while leaving the organizer's untouched.
    let writes = exec.writes();
    let put = writes
        .iter()
        .find(|w| w.method == DavMethod::Put)
        .expect("a PUT was issued");
    assert_eq!(
        put.precondition,
        Precondition::IfMatch("\"rt-v1\"".to_owned())
    );
    assert!(!put.body.contains("METHOD:"));
    let body = parse_calendar_object(
        &put.body,
        href.clone(),
        CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap(),
    )
    .expect("the PUT body is valid iCalendar");
    let my_status = &body
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some(me))
        .unwrap()
        .participation_status;
    assert_eq!(my_status, &ParticipationStatus::Accepted);
    let boss = body
        .participants
        .iter()
        .find(|p| p.email.as_deref() == Some("boss@test.local"))
        .unwrap();
    assert_eq!(boss.participation_status, ParticipationStatus::Accepted);
}

#[tokio::test]
async fn a_parsed_request_whose_organizer_mismatches_the_sender_is_rejected() {
    // The required security test (`calendar-semantics.md`), end to end on a *parsed*
    // message: the body's ORGANIZER is boss, but the authenticated sender is an
    // attacker — the bridge refuses it, so no write is ever planned.
    use crate::imip;
    use engine_core::scheduling::{ImipUntrusted, ScheduleAction, reconcile};

    let message = imip::parse(INVITE_REQUEST).expect("parse imip request");
    let action = reconcile(&message, Some("attacker@evil.example"), None);
    assert_eq!(
        action,
        ScheduleAction::Rejected(ImipUntrusted::SenderMismatch {
            expected: "organizer"
        })
    );
    assert!(
        !matches!(action, ScheduleAction::ScheduleEvent),
        "an untrusted invite is never scheduled"
    );
}
