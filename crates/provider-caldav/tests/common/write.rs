//! The live CalDAV **write** scenarios, run against every real server the harness
//! offers. Each one answers a question an offline fake structurally cannot, because a
//! fake replays canned bytes without ever reading the request:
//!
//! - [`round_trip`] — does the `PUT` hand back the new `ETag`, and is it the resource's real one
//!   (i.e. usable as the next precondition with no refetch)?
//! - [`patched_update_preserves_the_document`] — does an edit made through the neutral patch verb
//!   survive the **server**? Our byte-equality tests prove the *patcher* keeps the `RRULE`, the
//!   `VALARM`, the `VTIMEZONE` and the `X-` properties; they say nothing about whether the server
//!   stores them or quietly normalizes them away.
//! - [`stale_if_match_is_a_conflict`] — does a superseded guard really come back `412`, and does
//!   the adapter class it `Conflict` (refetch-and-merge) rather than a blind-retryable `Retryable`?
//! - [`instance_override_split_is_accepted`] — does a `RECURRENCE-ID` override the patcher splits
//!   out of a master get accepted as part of the same resource, and come back folded into one
//!   event?
//!
//! They are written the way a **host** writes: state the intent through the neutral verbs
//! (`engine_provider::EventDraft`/`EventEdit`) and never assemble iCalendar. The one
//! exception is seeding a fixture too rich for a draft to express (a `VTIMEZONE`, a
//! `VALARM`, an `RRULE`), which goes in through the whole-document verb — that is the
//! server-side fixture, not the thing under test.
//!
//! Every scenario leaves the seeded collection exactly as it found it.

use engine_core::{
    calendar::{Event, RecurrenceOverride},
    error::FailureClass,
    ids::{AccountId, Uid},
    raw::RawIcal,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
    version::RevisionTokens,
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventWrite, PatchTarget, Provider, WriteGuard,
};
use provider_caldav::CalDavProvider;

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

/// Seeds a document too rich for an [`EventDraft`] to express, through the whole-document
/// verb. This is fixture setup, not the thing under test.
async fn seed(provider: &CalDavProvider, account: &AccountId, uid: &Uid, body: String) {
    let href = provider.event_href(uid).expect("mint event href");
    provider
        .put_event(
            account,
            &EventWrite::unconditional(href, uid.clone(), RawIcal::new(body)),
        )
        .await
        .expect("seed the event");
}

/// The properties a patch is *allowed* to rewrite. Everything else must survive both the
/// patcher and the server byte for byte (RFC 5545 requires the `DTSTAMP`/`LAST-MODIFIED`
/// bookkeeping of a revision; `SEQUENCE` moves only on a significant change).
const PATCHABLE: &[&str] = &["SUMMARY", "DTSTAMP", "LAST-MODIFIED", "SEQUENCE"];

// ---------------------------------------------------------------------------
// 1. The ETag chain.
// ---------------------------------------------------------------------------

const ROUND_TRIP_UID: &str = "caldav-write-roundtrip@test.local";

/// The full write lifecycle — create → patch → delete — with the **delete guarded by the
/// `ETag` the patch `PUT` handed back**, never a refetched one.
///
/// That is the point, and it is what makes this more than a smoke test: `caldav.md`
/// promises a host can write, keep the receipt's revision, and write again without a round
/// trip to re-read it (RFC 4791 §5.3.4 only *recommends* the response `ETag`, and plenty of
/// servers omit it — the receipt's field is optional precisely because we could not
/// previously prove any server supplied it). It matters more than it looks: until the next
/// sync the **store still holds the pre-write revision**, so a host that re-read it there
/// would guard on a superseded one and get a `412` on a write that should have succeeded.
/// If the receipt's `ETag` were absent, stale, or not the resource's, this fails.
pub(crate) async fn round_trip(provider: &CalDavProvider, account: &AccountId) {
    let caps = provider.connection_info().capabilities;
    assert!(
        caps.calendar_writes(),
        "the CalDAV provider advertises calendar writes"
    );
    assert_eq!(
        caps.calendar_write_guard(),
        Some(WriteGuard::Enforced),
        "CalDAV is the transport that can actually promise a lost-update guard — and the \
         rest of this scenario is what earns that claim"
    );

    let uid = Uid::new(ROUND_TRIP_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    // ---- Create: the host states an event, the adapter serializes it. ----
    let created = provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Live write test",
                amsterdam("2026-06-01T10:00:00"),
                amsterdam("2026-06-01T11:00:00"),
                stamp(),
            ),
        )
        .await
        .expect("create event");
    assert_eq!(created.uid.as_str(), ROUND_TRIP_UID);
    let etag_v1 = created
        .revisions
        .etag
        .clone()
        .expect("the server returns the new ETag on the create PUT (RFC 4791 §5.3.4)");

    let made = require(provider, account, ROUND_TRIP_UID).await;
    assert_eq!(made.title, "Live write test");
    assert_eq!(
        made.start,
        amsterdam("2026-06-01T10:00:00"),
        "the create was born zoned — never flattened to the UTC instant it denotes today"
    );
    // The ETag the PUT reported *is* the resource's ETag — so it can be used as the next
    // precondition without re-reading the collection first.
    assert_eq!(
        server_etag(&made),
        etag_v1,
        "the PUT's ETag is the one the collection reports"
    );

    // ---- Patch, guarded by the revision we read. ----
    let updated = provider
        .patch_event(
            account,
            &made,
            &EventEdit::new(
                &made,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Live write test (edited)"),
            ),
        )
        .await
        .expect("patch the event");
    let etag_v2 = updated
        .revisions
        .etag
        .clone()
        .expect("the server returns the new ETag on the update PUT");
    assert_ne!(etag_v2, etag_v1, "the ETag moves when the resource changes");

    let edited = require(provider, account, ROUND_TRIP_UID).await;
    assert_eq!(edited.title, "Live write test (edited)");

    // ---- Delete, guarded by the ETag the *patch's receipt* carried — not one we went
    // back to the server for. This is the chain the host contract depends on.
    let mut as_written = made.clone();
    as_written.revisions = RevisionTokens::from_etag(etag_v2);
    provider
        .delete_event(account, &EventDeletion::of(&as_written))
        .await
        .expect("delete the event using the receipt's ETag, with no refetch");
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

/// Retitles a rich event through the neutral patch verb and proves the **server** kept
/// everything else — the claim the patcher makes and could not, until now, back.
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
    seed(provider, account, &uid, rich_body()).await;

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

    // ---- Retitle it, and nothing else. The host says only that; the adapter does the
    // surgery over the stored raw and PUTs the result under the revision it read.
    provider
        .patch_event(
            account,
            &before,
            &EventEdit::new(
                &before,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Weekly standup (renamed)"),
            ),
        )
        .await
        .expect("patch the server's stored document");

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
        .delete_event(account, &EventDeletion::of(&after))
        .await
        .expect("delete the rich event");
}

// ---------------------------------------------------------------------------
// 3. The 412 conflict.
// ---------------------------------------------------------------------------

const CONFLICT_UID: &str = "caldav-stale-etag@test.local";

/// A superseded guard must come back `412` and class as [`FailureClass::Conflict`] —
/// *not* `Retryable`.
///
/// The distinction is the whole recovery strategy: a `Conflict` means the server copy
/// moved on, so the stored `RawIcal` the patch was built from is stale and the edit must
/// be refetched, re-applied and resubmitted. A blind retry would either fail forever or,
/// worse, succeed by clobbering someone else's change.
///
/// This is also the assertion that earns CalDAV its [`WriteGuard::Enforced`]. The JMAP
/// adapter cannot pass it — which is why it advertises [`WriteGuard::Absent`] instead of
/// pretending (`jmap.md`).
pub(crate) async fn stale_if_match_is_a_conflict(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new(CONFLICT_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid.clone(),
                "Original",
                amsterdam("2026-06-03T10:00:00"),
                amsterdam("2026-06-03T11:00:00"),
                stamp(),
            ),
        )
        .await
        .expect("create event");

    // The copy a host read — and the revision it holds. Everything below patches from
    // *this* base, so `stale` is stale in exactly the way a real host's would be.
    let stale: Event = require(provider, account, CONFLICT_UID).await;

    // Someone (here: us) moves the server copy on. The revision `stale` carries no longer
    // exists.
    provider
        .patch_event(
            account,
            &stale,
            &EventEdit::new(
                &stale,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Moved on"),
            ),
        )
        .await
        .expect("the first update, from a current base, succeeds");

    // ---- The stale update: same base, whose revision the server has now superseded. ----
    let error = provider
        .patch_event(
            account,
            &stale,
            &EventEdit::new(
                &stale,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Clobber"),
            ),
        )
        .await
        .expect_err("a superseded guard must not overwrite the server copy");
    assert_eq!(
        error.class(),
        FailureClass::Conflict,
        "a 412 is a Conflict — refetch and merge, never a blind retry"
    );
    assert!(!error.is_retryable(), "a conflict is not blind-retryable");

    // ---- The stale delete: same precondition, same verdict. ----
    let error = provider
        .delete_event(account, &EventDeletion::of(&stale))
        .await
        .expect_err("a superseded guard must not delete the server copy");
    assert_eq!(error.class(), FailureClass::Conflict);

    // The event is untouched by both rejected writes: the edit that landed still stands.
    let survivor = require(provider, account, CONFLICT_UID).await;
    assert_eq!(survivor.title, "Moved on");

    provider
        .delete_event(
            account,
            &EventDeletion::unconditional(survivor.id, survivor.uid),
        )
        .await
        .expect("clean up");
}

// ---------------------------------------------------------------------------
// 4. Splitting a RECURRENCE-ID override out of a master.
// ---------------------------------------------------------------------------

const SERIES_UID: &str = "caldav-override-split@test.local";

/// Moving **one occurrence** of a series makes the CalDAV adapter split a fresh
/// `RECURRENCE-ID` override out of the master — a second `VEVENT` in the same resource.
/// This proves the server accepts that resource and hands it back folded into one event
/// with the override in place, leaving the rest of the series where it was.
///
/// The splitting is CalDAV's chore alone: a JMAP server materializes the override itself
/// from a `recurrenceOverrides/<start>/…` patch. Same neutral intent, entirely different
/// work underneath — which is the argument for `PatchTarget` living where it does.
pub(crate) async fn instance_override_split_is_accepted(
    provider: &CalDavProvider,
    account: &AccountId,
) {
    let uid = Uid::new(SERIES_UID).unwrap();
    pre_clean(provider, account, &uid).await;

    // Tuesdays at 10:00 Amsterdam: 2, 9, 16 and 23 June 2026. An `RRULE` is beyond what a
    // draft can state, so the series is seeded as a document.
    seed(
        provider,
        account,
        &uid,
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//engine//caldav-override-test//EN\r\n\
             BEGIN:VEVENT\r\nUID:{SERIES_UID}\r\nDTSTAMP:20260601T000000Z\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260602T100000\r\n\
             DTEND;TZID=Europe/Amsterdam:20260602T110000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=4;BYDAY=TU\r\n\
             SUMMARY:Standup\r\nX-CUSTOM-FLAG:keep-me\r\nSEQUENCE:0\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n"
        ),
    )
    .await;

    let before = require(provider, account, SERIES_UID).await;

    // Drag the *second* occurrence (9 June) from 10:00 to 14:00. `PatchTarget::Instance`
    // names it by the start it has **now** — its identity in the series, not its
    // destination — and a fresh split needs this occurrence's own start and end, because
    // the master's are the *first* occurrence's.
    provider
        .patch_event(
            account,
            &before,
            &EventEdit::new(
                &before,
                PatchTarget::Instance(amsterdam("2026-06-09T10:00:00")),
                EventPatch::new(stamp())
                    .summary("Standup (moved)")
                    .start(amsterdam("2026-06-09T14:00:00"))
                    .end(amsterdam("2026-06-09T15:00:00")),
            ),
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
        .delete_event(account, &EventDeletion::of(&after))
        .await
        .expect("delete the series");
}
