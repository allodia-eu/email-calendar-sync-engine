//! Gated live `CalendarEvent/set` checks against the Stalwart harness, mirroring the CalDAV
//! write suite (`provider-caldav/tests/common/write.rs`). Skips with no `STALWART_HTTP_ADDR`.
//!
//! Writes are where a live server is not optional. The offline `FakeExecutor` serves canned
//! bytes **whatever it is sent**, so a `CalendarEvent/set` with a bad JSON-pointer or a
//! malformed JSCalendar object passes every offline test and fails against a real server
//! (`AGENTS.md`). These answer what only a server can:
//!
//! - [`round_trip`] — does a create/patch/destroy actually land, and does the **server** hand back
//!   the id it assigned?
//! - [`partial_update_is_merged_by_the_server`] — the finding this whole adapter is built on. An
//!   `update` of one property must leave every other one alone. If Stalwart replaced the object
//!   instead of merging the patch, editing a title would silently wipe the event's zone, duration
//!   and recurrence — and no offline test would notice, because there is no document on our side to
//!   compare against.
//! - [`a_stale_edit_is_not_refused`] — the honesty check. It asserts the *absence* of a lost-update
//!   guard, which is what [`WriteGuard::Absent`] claims: the claim is pinned to observed behaviour,
//!   not to a reading of the spec. Note what it is blind to — it drives the adapter, which sends no
//!   precondition, so it cannot notice the server *gaining* one. Stalwart v0.16.14 started
//!   enforcing `ifInState` and this test passes unchanged on the pinned v0.16.15; see the test's
//!   own docs.
//! - [`recurrence_override_edit`] — is a `recurrenceOverrides/<start>/…` pointer accepted, and does
//!   the server materialize the override itself?
//!
//! Every scenario leaves the seeded calendar exactly as it found it.

use engine_core::{
    calendar::{Event, RecurrenceOverride},
    ids::{AccountId, CalendarId, Uid},
    sync::SyncUpdate,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget, Provider, WriteGuard,
};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

async fn connect(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect")
}

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 6, 1, 12, 0, 0).unwrap()
}

fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// Every event the account currently holds.
async fn all_events(provider: &JmapProvider) -> Vec<Event> {
    let events = provider.sync_events(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
        panic!("expected a snapshot");
    };
    objects
}

/// The event with `uid`, if the server still holds one.
async fn fetch(provider: &JmapProvider, uid: &str) -> Option<Event> {
    all_events(provider)
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == uid)
}

async fn require(provider: &JmapProvider, uid: &str) -> Event {
    fetch(provider, uid)
        .await
        .unwrap_or_else(|| panic!("event {uid} is present on the server"))
}

/// The account's first calendar — where a throwaway event lands.
async fn calendar(provider: &JmapProvider) -> CalendarId {
    let calendars = provider.sync_calendars(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = calendars.update else {
        panic!("expected a snapshot");
    };
    objects
        .into_iter()
        .next()
        .expect("the seeded account has a calendar")
        .id
}

/// Removes any residue of `uid` from a prior interrupted run.
async fn pre_clean(provider: &JmapProvider, uid: &str) {
    if let Some(stale) = fetch(provider, uid).await {
        provider
            .delete_event(&account(), &EventDeletion::of(&stale))
            .await
            .expect("clean up a prior run's event");
    }
}

/// Starts the harness, or `None` when the gate env var is unset.
async fn setup(name: &str) -> Option<JmapProvider> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {name}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    Some(connect(&harness).await)
}

// ---------------------------------------------------------------------------
// 1. The write lifecycle.
// ---------------------------------------------------------------------------

const ROUND_TRIP_UID: &str = "jmap-write-roundtrip@test.local";

#[tokio::test]
async fn round_trip() {
    let Some(provider) = setup("round_trip").await else {
        return;
    };
    let caps = provider.connection_info().capabilities;
    assert!(caps.calendars() && caps.calendar_writes());
    assert_eq!(
        caps.calendar_write_guard(),
        Some(WriteGuard::Absent),
        "JMAP advertises that it cannot refuse a stale write — see a_stale_edit_is_not_refused"
    );

    let uid = Uid::new(ROUND_TRIP_UID).unwrap();
    pre_clean(&provider, ROUND_TRIP_UID).await;

    // ---- Create. The *server* assigns the id. ----
    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                uid.clone(),
                "Live JMAP write test",
                amsterdam("2026-06-01T10:00:00"),
                amsterdam("2026-06-01T11:00:00"),
                stamp(),
            )
            // A create mints the `locations` map from nothing; the read below proves the
            // server stored it and `parse_locations` reads it back.
            .location("Room 6"),
        )
        .await
        .expect("create event");
    assert_eq!(created.uid, uid);
    assert!(
        created.revisions.is_empty(),
        "a JMAP object carries no per-object revision, and the receipt must not invent one"
    );

    let made = require(&provider, ROUND_TRIP_UID).await;
    assert_eq!(
        made.id, created.event,
        "the id the create reported is the id the server actually stored it under — the \
         only place a caller can learn it"
    );
    assert_eq!(made.title, "Live JMAP write test");
    assert_eq!(
        made.start,
        amsterdam("2026-06-01T10:00:00"),
        "the event was born zoned — the wall clock and the zone survived, and the create did \
         not flatten it to the UTC instant it happens to denote today"
    );
    assert_eq!(made.duration, "PT1H".parse().unwrap());
    assert_eq!(
        made.locations.first().and_then(|l| l.name.as_deref()),
        Some("Room 6"),
        "the location stated on the create survived the server and read back"
    );

    // ---- Patch: retitle and move it. ----
    provider
        .patch_event(
            &account(),
            &made,
            &EventEdit::new(
                &made,
                PatchTarget::Series,
                EventPatch::new(stamp())
                    .summary("Live JMAP write test (edited)")
                    .start(amsterdam("2026-06-01T14:00:00"))
                    .end(amsterdam("2026-06-01T15:30:00")),
            ),
        )
        .await
        .expect("patch the event");

    let edited = require(&provider, ROUND_TRIP_UID).await;
    assert_eq!(edited.title, "Live JMAP write test (edited)");
    assert_eq!(edited.start, amsterdam("2026-06-01T14:00:00"));
    assert_eq!(
        edited.duration,
        "PT1H30M".parse().unwrap(),
        "JSCalendar has no end; the adapter re-derived the duration from the new start"
    );

    // ---- Destroy. ----
    provider
        .delete_event(&account(), &EventDeletion::of(&edited))
        .await
        .expect("destroy the event");
    assert!(
        fetch(&provider, ROUND_TRIP_UID).await.is_none(),
        "the event is gone after the destroy"
    );

    // And destroying it again is still a success: the desired end state already holds, which
    // is what makes an outbox retry of a delete whose response was lost safe.
    provider
        .delete_event(&account(), &EventDeletion::of(&edited))
        .await
        .expect("destroying an already-gone event is idempotent success");
}

// ---------------------------------------------------------------------------
// 2. The server merges the patch. (The premise of the whole adapter.)
// ---------------------------------------------------------------------------

const MERGE_UID: &str = "jmap-partial-update@test.local";

/// An `update` of one property must leave every other one alone — the server merges the
/// PatchObject into the stored object rather than replacing it (RFC 8620 §5.3).
///
/// This is the JMAP counterpart of CalDAV's `patched_update_preserves_the_document`, and it
/// is load-bearing in exactly the same way: it is the *reason* this adapter has no JSCalendar
/// serializer and does no document surgery. If the server replaced instead of merged,
/// renaming an event would silently wipe its zone, its duration and its recurrence — a save
/// that looks like it worked. No offline test can catch that, because on this transport we
/// hold no document to compare against; only the server's copy can answer.
#[tokio::test]
async fn partial_update_is_merged_by_the_server() {
    let Some(provider) = setup("partial_update_is_merged_by_the_server").await else {
        return;
    };
    let uid = Uid::new(MERGE_UID).unwrap();
    pre_clean(&provider, MERGE_UID).await;

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                uid.clone(),
                "Before",
                amsterdam("2026-06-02T09:00:00"),
                amsterdam("2026-06-02T09:45:00"),
                stamp(),
            )
            .description("keep me"),
        )
        .await
        .expect("create event");
    let before = require(&provider, MERGE_UID).await;

    // Retitle it, and *only* that.
    provider
        .patch_event(
            &account(),
            &before,
            &EventEdit::new(
                &before,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("After"),
            ),
        )
        .await
        .expect("patch only the title");

    let after = require(&provider, MERGE_UID).await;
    assert_eq!(after.title, "After", "the edit landed");
    // Everything the patch never mentioned is exactly as it was.
    assert_eq!(after.uid, before.uid);
    assert_eq!(after.start, before.start, "the server kept the zoned start");
    assert_eq!(after.duration, before.duration, "the server kept duration");
    assert_eq!(
        after.description, before.description,
        "the server kept a property the patch never mentioned"
    );
    assert_eq!(after.calendars, before.calendars);

    provider
        .delete_event(&account(), &EventDeletion::of(&after))
        .await
        .expect("clean up");
}

// ---------------------------------------------------------------------------
// 3. There is no lost-update guard — asserted, not assumed.
// ---------------------------------------------------------------------------

const STALE_UID: &str = "jmap-stale-edit@test.local";

/// A write built on a **superseded** copy is applied anyway. That is the truth this
/// transport advertises as [`WriteGuard::Absent`], and here it is, live.
///
/// The CalDAV suite asserts the opposite (`stale_if_match_is_a_conflict`: a superseded
/// `If-Match` is a `412`), and the contrast is the whole reason the guard is a capability
/// rather than an assumption. Two independent reasons it cannot hold here, both established
/// rather than guessed:
///
/// 1. A `CalendarEvent` carries **no per-object revision** — no `ETag`, no `changeKey` — so there
///    is nothing to name *this* event's version with.
/// 2. `ifInState`, the only precondition RFC 8620 §5.3 offers, is scoped to the account's whole
///    `CalendarEvent` state, not to the object: on a compliant server it would reject our edit
///    because somebody added an *unrelated* meeting. It is the wrong instrument, not merely a
///    broken one — so we do not send it. (Stalwart ignored `ifInState` through v0.16.13 and
///    enforces it from v0.16.14; we send none on either, so neither vintage changes this.)
///
/// **If this test ever fails, that is good news and a required design change**: the server started
/// refusing the stale writes *we actually send*, and `session.rs` must stop advertising `Absent`.
///
/// It is blind to one thing these docs used to get wrong: the adapter sends no precondition, so
/// Stalwart *gaining* one cannot fail this test — the pinned v0.16.15 enforces `ifInState` (stale
/// token → `stateMismatch`) and this passes unchanged. Orthogonal anyway: it is account-scoped.
#[tokio::test]
async fn a_stale_edit_is_not_refused() {
    let Some(provider) = setup("a_stale_edit_is_not_refused").await else {
        return;
    };
    let uid = Uid::new(STALE_UID).unwrap();
    pre_clean(&provider, STALE_UID).await;

    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                uid.clone(),
                "Original",
                amsterdam("2026-06-03T10:00:00"),
                amsterdam("2026-06-03T11:00:00"),
                stamp(),
            ),
        )
        .await
        .expect("create event");

    // The copy a host read, and would build its next edit from.
    let stale = require(&provider, STALE_UID).await;

    // Somebody else (here: us) moves the server copy on.
    provider
        .patch_event(
            &account(),
            &stale,
            &EventEdit::new(
                &stale,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Someone else's edit"),
            ),
        )
        .await
        .expect("the first edit lands");

    // Now edit again from the **stale** base. On CalDAV this is a `412 Conflict`. Here it
    // succeeds, and the other writer's edit is gone.
    provider
        .patch_event(
            &account(),
            &stale,
            &EventEdit::new(
                &stale,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Clobber"),
            ),
        )
        .await
        .expect("a stale edit is NOT refused on this transport — this is what Absent means");

    let survivor = require(&provider, STALE_UID).await;
    assert_eq!(
        survivor.title, "Clobber",
        "last writer wins: the concurrent edit was silently lost. A host that must not lose \
         it has to detect that itself — which is precisely what calendar_write_guard tells \
         it, before it writes"
    );

    provider
        .delete_event(&account(), &EventDeletion::of(&survivor))
        .await
        .expect("clean up");
}

// ---------------------------------------------------------------------------
// 4. Editing one occurrence: the recurrenceOverrides pointer.
// ---------------------------------------------------------------------------

/// The seeded weekly series (Mondays 09:30 Amsterdam) already carries an override at
/// 2026-01-26, where the instance was moved to the afternoon.
const SERIES_UID: &str = "weekly-2002@test.local";
const OVERRIDE_AT: &str = "2026-01-26T09:30:00";
const SEED_OVERRIDE_TITLE: &str = "Weekly standup (this instance moved to the afternoon)";

/// Retitles **one occurrence** of a recurring event and proves the server accepted the
/// `recurrenceOverrides/<original start>/title` pointer.
///
/// This is where the two transports diverge most sharply under one neutral intent. CalDAV
/// has to *split a whole `RECURRENCE-ID` `VEVENT` out of the master by hand* — copying its
/// properties, dropping its `RRULE`, minting the recurrence id. JMAP just names the
/// occurrence in a JSON pointer and the **server** materializes the override. Same
/// `PatchTarget::Instance`; the work underneath is not remotely the same. That is the
/// argument for the patcher staying in `provider-caldav` while the *intent* is neutral.
///
/// A bad pointer is exactly the class of bug the offline fake cannot catch (it would reply
/// `updated` to a malformed patch just as readily), so this must run against a server.
///
/// It edits the **seed** — the only recurring event available, since a neutral `EventDraft`
/// cannot yet state a recurrence rule — and restores the original title before returning, so
/// the seed the read tests assert on is left exactly as found.
#[tokio::test]
async fn recurrence_override_edit() {
    let Some(provider) = setup("recurrence_override_edit").await else {
        return;
    };
    let series = require(&provider, SERIES_UID).await;
    assert!(series.is_recurring(), "the seeded series recurs");
    let override_at: CalendarDateTime = amsterdam(OVERRIDE_AT);

    let title_of = |event: &Event| -> String {
        let recurrence = event.recurrence.as_ref().expect("the series has a rule");
        let key = OVERRIDE_AT.parse().expect("a local date-time");
        let RecurrenceOverride::Patch(patch) = recurrence
            .overrides
            .get(&key)
            .expect("the seeded override at 2026-01-26")
        else {
            panic!("the seeded override is a patch, not an exclusion");
        };
        patch
            .get("title")
            .and_then(serde_json::Value::as_str)
            .expect("the override carries a title")
            .to_owned()
    };
    assert_eq!(title_of(&series), SEED_OVERRIDE_TITLE);

    // ---- Retitle just that occurrence. ----
    provider
        .patch_event(
            &account(),
            &series,
            &EventEdit::new(
                &series,
                PatchTarget::Instance(override_at.clone()),
                EventPatch::new(stamp()).summary("Standup (occurrence renamed)"),
            ),
        )
        .await
        .expect("the server accepts a recurrenceOverrides/<start>/title pointer");

    let edited = require(&provider, SERIES_UID).await;
    assert_eq!(
        title_of(&edited),
        "Standup (occurrence renamed)",
        "the override's title changed"
    );
    assert_eq!(
        edited.title, "Weekly standup",
        "and the SERIES title did not — the patch landed on the occurrence, not the master"
    );
    assert_eq!(
        edited.start, series.start,
        "the series' own start is untouched"
    );

    // ---- Restore the seed. ----
    provider
        .patch_event(
            &account(),
            &edited,
            &EventEdit::new(
                &edited,
                PatchTarget::Instance(override_at),
                EventPatch::new(stamp()).summary(SEED_OVERRIDE_TITLE),
            ),
        )
        .await
        .expect("restore the seeded override title");
    assert_eq!(
        title_of(&require(&provider, SERIES_UID).await),
        SEED_OVERRIDE_TITLE,
        "the seed is left exactly as it was found"
    );
}
