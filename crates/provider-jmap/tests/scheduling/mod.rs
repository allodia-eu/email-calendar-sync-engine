//! The two-party fixture the live scheduling scenarios share: a real invitation between two
//! real accounts on the Stalwart harness, placed over CalDAV and worked over JMAP.
//!
//! # Why the invitation is seeded over CalDAV
//!
//! JMAP has no whole-document write, and `EventDraft` cannot state an `ORGANIZER`/`ATTENDEE`
//! pair — so there is no way to *create* an invitation over JMAP alone. The organizer's copy
//! is therefore placed with a CalDAV `PUT`, exactly as `provider-caldav`'s scheduling
//! fixture does, and Stalwart's RFC 6638 auto-scheduling delivers it to the attendee. That
//! is the counterparty's fixture, not the thing under test: **every assertion in the
//! scenarios is about what JMAP did**.
//!
//! It also makes the tests stronger than same-protocol ones would be. The invitation arrives
//! by one protocol and is worked by another, against one server — so the participant ids the
//! JMAP projection sees really do address the resource CalDAV wrote, and the two protocols'
//! scheduling behaviour can be compared with everything else held constant.
//!
//! The organizer therefore holds **both** clients: CalDAV to place the invitation, JMAP for
//! the scenario that has them cancel it.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    ids::{AccountId, Uid},
    raw::RawIcal,
    scheduling::addresses_match,
    sync::SyncUpdate,
};
use engine_provider::{EventDeletion, EventWrite, Provider};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials as DavCredentials};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, ScratchAccount};

/// Each test gets its **own** UID, derived from its name.
///
/// They share two scratch accounts and run concurrently, so a single shared UID means one
/// test's cleanup deletes the other's invitation mid-flight — which is exactly how this
/// first failed in CI while passing locally, where they had been run one at a time.
fn uid_for(test: &str) -> String {
    format!("jmap-sched-{test}@test.local")
}

/// How long to wait for an asynchronous iTIP delivery. A poll on real state, never a sleep.
pub(crate) const DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long the server gets to schedule a message before a scenario records that it did not.
///
/// The notified path lands in well under a second, so this is generous by an order of
/// magnitude — an absence claimed too early would be a flake, and those are load-bearing.
pub(crate) const ORGANIZER_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) fn organizer_account() -> AccountId {
    AccountId::try_from("jmap-sched-organizer").unwrap()
}

pub(crate) fn attendee_account() -> AccountId {
    AccountId::try_from("jmap-sched-attendee").unwrap()
}

/// The two parties. The organizer speaks CalDAV to *place* the invitation and JMAP to cancel
/// it; the attendee speaks JMAP throughout, and is the one under test.
pub(crate) struct Parties {
    pub(crate) organizer: CalDavProvider,
    pub(crate) organizer_jmap: JmapProvider,
    pub(crate) organizer_address: String,
    pub(crate) attendee: JmapProvider,
    pub(crate) attendee_address: String,
    /// This test's own invitation UID — see [`uid_for`].
    pub(crate) uid: String,
    /// Kept for the raw DAV seam alone: the scheduling-inbox sweep in [`clean_up`] has no
    /// provider-level equivalent to go through.
    harness: Harness,
    organizer_auth: ScratchAccount,
    attendee_auth: ScratchAccount,
}

pub(crate) async fn parties(test: &str) -> Option<Parties> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let base = format!("http://{}", harness.http_addr);
    let [organizer_auth, attendee_auth] = harness.scratch.clone();

    let organizer = CalDavProvider::connect(CalDavConfig::new(
        base.clone(),
        DavCredentials::Basic {
            username: organizer_auth.address.clone(),
            password: organizer_auth.password.clone(),
        },
    ))
    .await
    .expect("organizer connects over CalDAV");

    let organizer_jmap = connect_jmap(&base, &organizer_auth).await;
    let attendee = connect_jmap(&base, &attendee_auth).await;

    Some(Parties {
        uid: uid_for(test),
        organizer_address: organizer_auth.address.clone(),
        attendee_address: attendee_auth.address.clone(),
        organizer,
        organizer_jmap,
        attendee,
        harness,
        organizer_auth,
        attendee_auth,
    })
}

async fn connect_jmap(base: &str, account: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(JmapConfig::new(
        base.to_owned(),
        Credentials::basic(&account.address, &account.password),
    ))
    .await
    .expect("connects over JMAP")
}

/// How far ahead of today the invitation is scheduled. Any comfortably-future offset does;
/// two months is well clear of a slow CI queue and of a run started just before midnight.
const INVITATION_DAYS_AHEAD: i64 = 60;

/// The invitation's date, as `YYYYMMDD`, a fixed offset from **today**.
///
/// Deliberately not a fixed absolute 2026 date, unlike every other fixture in this harness.
/// The invitation reaches the attendee only because the server decides to auto-schedule it,
/// and Stalwart does not deliver an iTIP message for a meeting that has already finished —
/// so a hard-coded date here buys no determinism, it sets a timer. The original
/// `20260812T140000` passed every run until 15:00 on 12 August 2026 and failed all four
/// scenarios on every run after, with a delivery timeout that reads like a broken server.
/// `provider-caldav`'s scheduling fixture was written the same way and burned the same way
/// two days earlier; this suite predates that fix and never received it.
///
/// Nothing here asserts an absolute instant — the scenarios assert delivery, `PARTSTAT`
/// transitions and cancellation — so moving the day costs no determinism the suite relied on.
fn invitation_date() -> String {
    // Fully qualified: `Duration` in this module is `core::time::Duration` (the poll
    // timeouts), and only `time`'s carries a calendar-aware `days`.
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(INVITATION_DAYS_AHEAD);
    format!(
        "{:04}{:02}{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The organizer's invitation document. Assembled by hand for the same reason the CalDAV
/// fixture does it: a draft cannot state an `ORGANIZER`/`ATTENDEE` pair. The times of day
/// are fixed; the day itself comes from [`invitation_date`], which explains why.
fn invitation(parties: &Parties) -> String {
    let day = invitation_date();
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Harness//JMAP scheduling//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260701T080000Z\r\nSEQUENCE:0\r\n\
         DTSTART;TZID=Europe/Amsterdam:{day}T140000\r\n\
         DTEND;TZID=Europe/Amsterdam:{day}T150000\r\n\
         SUMMARY:Scheduling over JMAP\r\n\
         ORGANIZER;CN=Bob Tester:mailto:{organizer}\r\n\
         ATTENDEE;CN=Carol;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:\
         {attendee}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        uid = parties.uid,
        organizer = parties.organizer_address,
        attendee = parties.attendee_address,
    )
}

/// Every event an account currently holds, over whichever protocol it speaks.
pub(crate) async fn jmap_events(provider: &JmapProvider, account: &AccountId) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(account, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("expected a snapshot");
    };
    objects
}

pub(crate) async fn dav_events(provider: &CalDavProvider, account: &AccountId) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(account, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("expected a snapshot");
    };
    objects
}

/// The participant whose calendar address is `address`, by the one shared comparison.
pub(crate) fn participant(event: &Event, address: &str) -> ParticipationStatus {
    event
        .participants
        .iter()
        .find(|p| {
            p.email
                .as_deref()
                .is_some_and(|email| addresses_match(email, address))
        })
        .unwrap_or_else(|| panic!("a participant for {address}: {:?}", event.participants))
        .participation_status
        .clone()
}

/// The attendee's copy of this test's invitation, if they still hold one.
pub(crate) async fn attendee_copy(parties: &Parties) -> Option<Event> {
    jmap_events(&parties.attendee, &attendee_account())
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == parties.uid)
}

/// Polls the attendee's JMAP calendar until `ready` accepts the event, or fails.
pub(crate) async fn poll_jmap(
    parties: &Parties,
    what: &str,
    ready: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if let Some(event) = attendee_copy(parties).await
            && ready(&event)
        {
            return event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {DELIVERY_TIMEOUT:?} waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The organizer's CalDAV copy after giving the server `settle` to schedule anything it
/// meant to — a different account's resource we never wrote to.
///
/// A fixed wait rather than a poll, because this is only used to assert an **absence**:
/// there is no state to poll toward, and returning early would only ever be on the outcome
/// being recorded as not happening. The notified path lands in well under a second, so this
/// is generous by an order of magnitude.
pub(crate) async fn organizer_copy_after(parties: &Parties, settle: std::time::Duration) -> Event {
    tokio::time::sleep(settle).await;
    dav_events(&parties.organizer, &organizer_account())
        .await
        .into_iter()
        .find(|e| e.uid.as_str() == parties.uid)
        .expect("the organizer still holds their own copy")
}

/// Polls the **organizer's** copy — the one in the other account, which we never write to —
/// until `ready` accepts it. This is where the iTIP `REPLY` shows up, so a presence is polled
/// for rather than waited on.
pub(crate) async fn poll_organizer(
    parties: &Parties,
    what: &str,
    ready: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if let Some(event) = dav_events(&parties.organizer, &organizer_account())
            .await
            .into_iter()
            .find(|e| e.uid.as_str() == parties.uid)
            && ready(&event)
        {
            return event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {DELIVERY_TIMEOUT:?} waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Places the organizer's invitation and waits for it to reach the attendee over JMAP,
/// unanswered. The shared opening of every scenario.
pub(crate) async fn deliver_invitation(parties: &Parties) -> Event {
    let href = parties
        .organizer
        .event_href(&Uid::new(parties.uid.clone()).unwrap())
        .expect("mint event href");
    parties
        .organizer
        .put_event(
            &organizer_account(),
            &EventWrite::unconditional(
                href,
                Uid::new(parties.uid.clone()).unwrap(),
                RawIcal::new(invitation(parties)),
            ),
        )
        .await
        .expect("the organizer stores the invitation");

    let mine = poll_jmap(
        parties,
        "the invitation to reach the attendee over JMAP",
        |_| true,
    )
    .await;
    assert_eq!(
        participant(&mine, &parties.attendee_address),
        ParticipationStatus::NeedsAction,
        "the delivered invitation must start unanswered, or this proves nothing"
    );
    mine
}

/// The hrefs of `who`'s RFC 6638 scheduling-inbox messages that carry `uid`.
///
/// Read over the harness's raw DAV helpers on purpose: exposing the inbox as a
/// provider-level `REPORT` is a documented deferral (`calendar-semantics.md`), and a test
/// must not invent the feature it is meant to clean up after. Same shape as
/// `provider-caldav`'s sweep, which found this leak first.
fn inbox_hrefs_of(harness: &Harness, who: (&str, &str), address: &str, uid: &str) -> Vec<String> {
    let Ok(listing) = harness.dav_propfind_as(who, &Harness::scheduling_inbox_path_of(address))
    else {
        return Vec::new();
    };
    let body = String::from_utf8_lossy(&listing.body).into_owned();
    body.split("<D:href>")
        .skip(1)
        .filter_map(|chunk| chunk.split("</D:href>").next())
        // A Depth-1 PROPFIND lists the collection itself alongside its members; only the
        // members (the non-trailing-slash hrefs) are iTIP messages.
        .filter(|href| !href.ends_with('/'))
        .map(|href| href.replace("%40", "@"))
        // Read as the inbox's *owner*: the other party has no access, and a failed GET here
        // would silently match nothing and so delete nothing.
        .filter(|href| {
            harness
                .dav_get_as(who, href)
                .is_ok_and(|resource| String::from_utf8_lossy(&resource.body).contains(uid))
        })
        .collect()
}

/// Removes both parties' copies **and their scheduling-inbox residue** — an attendee's copy
/// outlives the organizer's on an auto-schedule server, so cleanup has to delete each side.
///
/// The inbox sweep is not optional tidiness. Every invitation deposits a `REQUEST` for the
/// attendee, every answered one a `REPLY` for the organizer, and every cancellation a
/// `CANCEL` — none of which a calendar delete removes. Measured against v0.16.15 before this
/// sweep existed: one run of this suite left the organizer +1 and the attendee +7 messages,
/// growing without bound on a long-lived local harness. CI never sees it because CI always
/// starts from `down -v`; a developer running the suite repeatedly does. `provider-caldav`'s
/// scheduling fixture hit exactly this first.
pub(crate) async fn clean_up(parties: &Parties) {
    for event in dav_events(&parties.organizer, &organizer_account())
        .await
        .into_iter()
        .filter(|e| e.uid.as_str() == parties.uid)
    {
        let _ = parties
            .organizer
            .delete_event(&organizer_account(), &EventDeletion::of(&event))
            .await;
    }
    for event in jmap_events(&parties.attendee, &attendee_account())
        .await
        .into_iter()
        .filter(|e| e.uid.as_str() == parties.uid)
    {
        let _ = parties
            .attendee
            .delete_event(&attendee_account(), &EventDeletion::of(&event))
            .await;
    }
    for who in [&parties.organizer_auth, &parties.attendee_auth] {
        for href in inbox_hrefs_of(&parties.harness, who.auth(), &who.address, &parties.uid) {
            let _ = parties.harness.dav_delete_as(who.auth(), &href);
        }
    }
}
