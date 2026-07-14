//! The live CalDAV **write** scenarios, run against every real server the harness
//! offers. Each one answers a question an offline fake structurally cannot, because a
//! fake replays canned bytes without ever reading the request:
//!
//! - [`round_trip`] — does the `PUT` hand back the new `ETag`, and is it the resource's real one
//!   (i.e. usable as the next `If-Match` with no refetch)?
//! - [`patched_update_preserves_the_document`] — does an edit made with the structural patcher
//!   survive the **server**? Our byte-equality tests prove the *patcher* keeps the `RRULE`, the
//!   `VALARM`, the `VTIMEZONE` and the `X-` properties; they say nothing about whether the server
//!   stores them or quietly normalizes them away.
//! - [`stale_if_match_is_a_conflict`] — does a superseded `If-Match` really come back `412`, and
//!   does the adapter class it `Conflict` (refetch-and-merge) rather than a blind-retryable
//!   `Retryable`?
//! - [`instance_override_split_is_accepted`] — does a `RECURRENCE-ID` override the patcher splits
//!   out of a master get accepted as part of the same resource, and come back folded into one
//!   event?
//!
//! Every scenario leaves the seeded collection exactly as it found it.

use engine_core::{
    calendar::RecurrenceOverride,
    error::FailureClass,
    ids::{AccountId, Uid},
    raw::RawIcal,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{EventDeletion, EventWrite, Provider};
use provider_caldav::{CalDavProvider, EventPatch, PatchTarget, patch_event_ical};

use super::{fetch, lines_without, pre_clean, require, server_etag, server_ical};

/// The `DTSTAMP`/`LAST-MODIFIED` a revision carries. Fixed, because engine time types
/// cannot read a clock — and a fixed value keeps the tests deterministic.
fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

/// A wall-clock time in the series' zone — the form a zoned event's start must keep.
fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// The properties a patch is *allowed* to rewrite. Everything else must survive both the
/// patcher and the server byte for byte (RFC 5545 requires the `DTSTAMP`/`LAST-MODIFIED`
/// bookkeeping of a revision; `SEQUENCE` moves only on a significant change).
const PATCHABLE: &[&str] = &["SUMMARY", "DTSTAMP", "LAST-MODIFIED", "SEQUENCE"];

// ---------------------------------------------------------------------------
// 1. The ETag chain.
// ---------------------------------------------------------------------------

const ROUND_TRIP_UID: &str = "caldav-write-roundtrip@test.local";

/// One iCalendar body, with `title` as the `SUMMARY` and `sequence` as the `SEQUENCE`.
fn simple_body(title: &str, sequence: u32) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-write-test//EN\r\n\
         BEGIN:VEVENT\r\nUID:{ROUND_TRIP_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260601T100000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260601T110000\r\n\
         SEQUENCE:{sequence}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// The full write lifecycle — create → update → delete — driven **entirely off the
/// `ETag`s the `PUT`s hand back**, never a refetched one.
///
/// That is the point, and it is what makes this more than a smoke test: `caldav.md`
/// promises a host can write, take the receipt's `ETag`, and write again without a
/// round trip to re-read it (RFC 4791 §5.3.4 *recommends* the response `ETag`, and
/// plenty of servers omit it — the receipt's field is an `Option` precisely because we
/// could not previously prove any server supplied it). If the receipt's `ETag` were
/// absent, stale, or not the resource's, the next `If-Match` would `412` and this fails.
///
/// The **href** still comes from a fresh sync, not from the minted create href: a server
/// is entitled to canonicalize the resource name, and a real host writes back to the
/// href it read.
pub(crate) async fn round_trip(provider: &CalDavProvider, account: &AccountId) {
    assert!(
        provider.connection_info().capabilities.calendar_writes(),
        "the CalDAV provider advertises calendar writes"
    );
    let uid = Uid::new(ROUND_TRIP_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    // ---- Create (If-None-Match: *). ----
    let created = provider
        .put_event(
            account,
            &EventWrite::create(
                provider.event_href(&uid).expect("mint event href"),
                uid.clone(),
                RawIcal::new(simple_body("Live write test", 0)),
            ),
        )
        .await
        .expect("create event");
    assert_eq!(created.uid.as_str(), ROUND_TRIP_UID);
    let etag_v1 = created
        .etag
        .clone()
        .expect("the server returns the new ETag on the create PUT (RFC 4791 §5.3.4)");

    let made = require(provider, account, ROUND_TRIP_UID).await;
    assert_eq!(made.title, "Live write test");
    // The ETag the PUT reported *is* the resource's ETag — so it can be used as the
    // next precondition without re-reading the collection first.
    assert_eq!(
        server_etag(&made),
        etag_v1,
        "the PUT's ETag is the one the collection reports"
    );
    let href = made.id.clone();

    // ---- Update, guarded by the ETag the create PUT returned. ----
    let updated = provider
        .put_event(
            account,
            &EventWrite::update(
                href.clone(),
                uid.clone(),
                RawIcal::new(simple_body("Live write test (edited)", 1)),
                etag_v1.clone(),
            ),
        )
        .await
        .expect("update event with the create's ETag");
    let etag_v2 = updated
        .etag
        .clone()
        .expect("the server returns the new ETag on the update PUT");
    assert_ne!(etag_v2, etag_v1, "the ETag moves when the resource changes");

    let edited = require(provider, account, ROUND_TRIP_UID).await;
    assert_eq!(edited.title, "Live write test (edited)");

    // ---- Delete, guarded by the ETag the update PUT returned. ----
    provider
        .delete_event(account, &EventDeletion::if_match(href, etag_v2))
        .await
        .expect("delete event with the update's ETag");
    assert!(
        fetch(provider, account, ROUND_TRIP_UID).await.is_none(),
        "the event is gone from the collection after the delete"
    );
}

// ---------------------------------------------------------------------------
// 2. Server-side preservation of a patched document.
// ---------------------------------------------------------------------------

const RICH_UID: &str = "caldav-patch-preserve@test.local";

/// An event carrying everything the lossy JSCalendar projection cannot express: an
/// embedded `VTIMEZONE`, an `RRULE`, a `VALARM`, an `X-` property, an `ORGANIZER` and
/// an `ATTENDEE` with parameters, and a folded `DESCRIPTION`. Re-serializing the
/// projection would destroy all of it; the patcher must not, and neither must the server.
fn rich_body() -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-preserve-test//EN\r\n\
         BEGIN:VTIMEZONE\r\nTZID:Europe/Amsterdam\r\n\
         BEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\n\
         TZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nTZNAME:CET\r\n\
         END:STANDARD\r\n\
         BEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\nTZOFFSETFROM:+0100\r\n\
         TZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nTZNAME:CEST\r\n\
         END:DAYLIGHT\r\nEND:VTIMEZONE\r\n\
         BEGIN:VEVENT\r\nUID:{RICH_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
         LAST-MODIFIED:20260601T000000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260602T100000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260602T110000\r\n\
         RRULE:FREQ=WEEKLY;COUNT=4;BYDAY=TU\r\n\
         SUMMARY:Weekly standup\r\n\
         DESCRIPTION:A description long enough that it must be folded across two phys\r\n \
         ical lines\r\n\
         X-CUSTOM-FLAG:keep-me\r\n\
         ORGANIZER;CN=Alice:mailto:alice@test.local\r\n\
         ATTENDEE;CN=Bob;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@test.local\r\n\
         SEQUENCE:0\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Reminder\r\n\
         END:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// Edits one property of a rich event with the structural patcher and proves the
/// **server** kept everything else — the claim #58 makes and could not, until now, back.
///
/// The comparison is between the server's copy *before* the patch and the server's copy
/// *after* it, with only the properties the patch may touch struck out. That isolates
/// the question "did anything get dropped?" from "did the server reformat?", which
/// matters because the two harness servers answer the second differently: SabreDAV
/// stores the bytes verbatim, Stalwart reserializes them. A server that silently drops
/// the `VALARM` or the `X-` property fails here — and *only* here; no offline fake can
/// tell you.
pub(crate) async fn patched_update_preserves_the_document(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    let uid = Uid::new(RICH_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    provider
        .put_event(
            account,
            &EventWrite::create(
                provider.event_href(&uid).expect("mint event href"),
                uid.clone(),
                RawIcal::new(rich_body()),
            ),
        )
        .await
        .expect("create the rich event");

    // What the server stored — the only copy that matters. The patch is applied to
    // *this*, exactly as a host would apply it to what it synced.
    let before = require(provider, account, RICH_UID).await;
    let stored = server_ical(&before);
    assert_eq!(before.title, "Weekly standup");
    assert!(before.is_recurring(), "the RRULE survived the create");
    for property in ["BEGIN:VTIMEZONE", "BEGIN:VALARM", "TRIGGER:-PT15M"] {
        assert!(
            stored.as_str().contains(property),
            "the server kept {property} on the create"
        );
    }
    assert!(
        stored.as_str().contains("X-CUSTOM-FLAG:keep-me"),
        "the server kept the X- property on the create"
    );

    // ---- Retitle it, and nothing else. ----
    let patched = patch_event_ical(
        &stored,
        &PatchTarget::Series,
        &EventPatch::new(stamp()).summary("Weekly standup (renamed)"),
    )
    .expect("patch the server's stored document");

    provider
        .put_event(
            account,
            &EventWrite::update(
                before.id.clone(),
                uid.clone(),
                patched,
                server_etag(&before),
            ),
        )
        .await
        .expect("PUT the patched document");

    let after = require(provider, account, RICH_UID).await;
    assert_eq!(after.title, "Weekly standup (renamed)", "the edit landed");

    // The lock: strike the properties the patch was allowed to rewrite, and the two
    // server copies must be line-for-line identical. Nothing was normalized away.
    let stored_after = server_ical(&after);
    assert_eq!(
        lines_without(stored.as_str(), PATCHABLE),
        lines_without(stored_after.as_str(), PATCHABLE),
        "the server dropped or rewrote a line the patch never touched"
    );

    provider
        .delete_event(
            account,
            &EventDeletion::if_match(after.id.clone(), server_etag(&after)),
        )
        .await
        .expect("delete the rich event");
}

// ---------------------------------------------------------------------------
// 3. The 412 conflict.
// ---------------------------------------------------------------------------

const CONFLICT_UID: &str = "caldav-stale-etag@test.local";

/// A superseded `If-Match` must come back `412` and class as
/// [`FailureClass::Conflict`] — *not* `Retryable`.
///
/// The distinction is the whole recovery strategy: a `Conflict` means the server copy
/// moved on, so the stored `RawIcal` the patch was built from is stale and the edit must
/// be refetched, re-patched and resubmitted. A blind retry would either fail forever or,
/// worse, succeed by clobbering someone else's change.
pub(crate) async fn stale_if_match_is_a_conflict(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new(CONFLICT_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let body = |summary: &str| {
        RawIcal::new(format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-conflict-test//EN\r\n\
             BEGIN:VEVENT\r\nUID:{CONFLICT_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260603T100000\r\n\
             DTEND;TZID=Europe/Amsterdam:20260603T110000\r\n\
             SUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        ))
    };

    let created = provider
        .put_event(
            account,
            &EventWrite::create(
                provider.event_href(&uid).expect("mint event href"),
                uid.clone(),
                body("Original"),
            ),
        )
        .await
        .expect("create event");
    let stale = created.etag.clone().expect("the create returns an ETag");
    let href = require(provider, account, CONFLICT_UID).await.id;

    // Someone (here: us) moves the server copy on. `stale` now names a revision that no
    // longer exists.
    provider
        .put_event(
            account,
            &EventWrite::update(href.clone(), uid.clone(), body("Moved on"), stale.clone()),
        )
        .await
        .expect("the first update, with a current ETag, succeeds");

    // ---- The stale update. ----
    let error = provider
        .put_event(
            account,
            &EventWrite::update(href.clone(), uid.clone(), body("Clobber"), stale.clone()),
        )
        .await
        .expect_err("a superseded If-Match must not overwrite the server copy");
    assert_eq!(
        error.class(),
        FailureClass::Conflict,
        "a 412 is a Conflict — refetch and merge, never a blind retry"
    );
    assert!(!error.is_retryable(), "a conflict is not blind-retryable");

    // ---- The stale delete: same precondition, same verdict. ----
    let error = provider
        .delete_event(account, &EventDeletion::if_match(href.clone(), stale))
        .await
        .expect_err("a superseded If-Match must not delete the server copy");
    assert_eq!(error.class(), FailureClass::Conflict);

    // The event is untouched by both rejected writes: the edit that landed still stands.
    let survivor = require(provider, account, CONFLICT_UID).await;
    assert_eq!(survivor.title, "Moved on");

    provider
        .delete_event(account, &EventDeletion::unconditional(href))
        .await
        .expect("clean up");
}

// ---------------------------------------------------------------------------
// 4. Splitting a RECURRENCE-ID override out of a master.
// ---------------------------------------------------------------------------

const SERIES_UID: &str = "caldav-override-split@test.local";

/// Moving **one occurrence** of a series makes the patcher split a fresh `RECURRENCE-ID`
/// override out of the master — a second `VEVENT` in the same resource. This proves the
/// server accepts that resource and hands it back folded into one event with the
/// override in place, leaving the rest of the series where it was.
pub(crate) async fn instance_override_split_is_accepted(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    let uid = Uid::new(SERIES_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    // Tuesdays at 10:00 Amsterdam: 2, 9, 16 and 23 June 2026.
    let series = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-override-test//EN\r\n\
         BEGIN:VEVENT\r\nUID:{SERIES_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20260602T100000\r\n\
         DTEND;TZID=Europe/Amsterdam:20260602T110000\r\n\
         RRULE:FREQ=WEEKLY;COUNT=4;BYDAY=TU\r\n\
         SUMMARY:Standup\r\nX-CUSTOM-FLAG:keep-me\r\nSEQUENCE:0\r\nEND:VEVENT\r\n\
         END:VCALENDAR\r\n"
    );
    provider
        .put_event(
            account,
            &EventWrite::create(
                provider.event_href(&uid).expect("mint event href"),
                uid.clone(),
                RawIcal::new(series),
            ),
        )
        .await
        .expect("create the series");

    let before = require(provider, account, SERIES_UID).await;

    // Drag the *second* occurrence (9 June) from 10:00 to 14:00. `PatchTarget::Instance`
    // names it by the start it has **now** — its identity in the series, not its
    // destination — and a fresh split needs this occurrence's own start and end, because
    // the master's are the *first* occurrence's.
    let moved = patch_event_ical(
        &server_ical(&before),
        &PatchTarget::Instance(amsterdam("2026-06-09T10:00:00")),
        &EventPatch::new(stamp())
            .summary("Standup (moved)")
            .start(amsterdam("2026-06-09T14:00:00"))
            .end(amsterdam("2026-06-09T15:00:00")),
    )
    .expect("split a RECURRENCE-ID override out of the master");

    provider
        .put_event(
            account,
            &EventWrite::update(before.id.clone(), uid.clone(), moved, server_etag(&before)),
        )
        .await
        .expect("the server accepts a master + RECURRENCE-ID override in one resource");

    // Back from the server: still ONE event (RFC 4791 §4.1 — every VEVENT in a resource
    // shares the UID), the master untouched, the override folded in beside it.
    let after = require(provider, account, SERIES_UID).await;
    assert_eq!(after.title, "Standup", "the master keeps its own title");
    assert!(after.is_recurring());
    assert!(
        server_ical(&after)
            .as_str()
            .contains("X-CUSTOM-FLAG:keep-me"),
        "the split copied the master's properties, and the server kept them"
    );

    let recurrence = after.recurrence.as_ref().expect("the series survived");
    let override_at = "2026-06-09T10:00:00".parse().expect("a local date-time");
    let RecurrenceOverride::Patch(patch) = recurrence
        .overrides
        .get(&override_at)
        .expect("the moved occurrence is an override, keyed by its original start")
    else {
        panic!("the moved occurrence is a patch, not an exclusion");
    };
    assert_eq!(
        patch.get("title").and_then(serde_json::Value::as_str),
        Some("Standup (moved)"),
        "the override carries the moved occurrence's new title"
    );

    provider
        .delete_event(
            account,
            &EventDeletion::if_match(after.id.clone(), server_etag(&after)),
        )
        .await
        .expect("delete the series");
}
