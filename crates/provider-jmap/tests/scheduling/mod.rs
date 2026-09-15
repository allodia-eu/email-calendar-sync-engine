//! The two-party fixture shared by the live JMAP scheduling scenarios. The organiser creates
//! an invitation over JMAP, then the attendee answers it over JMAP. CalDAV reads the
//! organiser's copy and removes scheduling inbox residue that JMAP does not expose.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    ids::{AccountId, Uid},
    scheduling::addresses_match,
    sync::SyncUpdate,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    CalendarAddress, CalendarWrites, EventDeletion, EventDraft, Invitee, MeetingDraft, Provider,
    SchedulingIdentity,
};
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

/// The two parties. Both write through JMAP. CalDAV observes the organiser's copy and cleans
/// up protocol resources that JMAP does not expose.
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

fn invitation_time(hour: u8) -> CalendarDateTime {
    let day = invitation_date();
    let local = format!(
        "{}-{}-{}T{hour:02}:00:00",
        &day[0..4],
        &day[4..6],
        &day[6..8]
    )
    .parse()
    .unwrap();
    CalendarDateTime::Zoned {
        local,
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
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
    let SyncUpdate::Snapshot { objects, .. } = parties
        .organizer_jmap
        .sync_calendars(&organizer_account(), None)
        .await
        .expect("sync organiser calendars")
        .update
    else {
        panic!("expected a calendar snapshot");
    };
    let calendar = objects
        .into_iter()
        .next()
        .expect("organiser has a calendar");
    let draft = EventDraft::new(
        calendar.id,
        Uid::new(parties.uid.clone()).unwrap(),
        "Scheduling over JMAP",
        invitation_time(14),
        invitation_time(15),
        UtcDateTime::new(2026, 7, 1, 8, 0, 0).unwrap(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::named(
            CalendarAddress::parse(&parties.organizer_address).unwrap(),
            "Bob Tester",
        ),
        vec![Invitee::required(SchedulingIdentity::named(
            CalendarAddress::parse(&parties.attendee_address).unwrap(),
            "Carol",
        ))],
    ));
    parties
        .organizer_jmap
        .create_event(&organizer_account(), &draft)
        .await
        .expect("the organiser creates the invitation over JMAP");

    let created = jmap_events(&parties.organizer_jmap, &organizer_account())
        .await
        .into_iter()
        .find(|event| event.uid.as_str() == parties.uid)
        .expect("the organiser can read back the created invitation");
    let raw: serde_json::Value = serde_json::from_str(
        created
            .raw_jscalendar
            .as_ref()
            .expect("the JMAP adapter preserves the raw event")
            .as_str(),
    )
    .expect("the preserved event is JSON");
    assert_eq!(
        raw["organizerCalendarAddress"],
        format!("mailto:{}", parties.organizer_address)
    );
    assert_eq!(
        created
            .participants
            .iter()
            .filter(|participant| participant
                .email
                .as_deref()
                .is_some_and(|address| { addresses_match(address, &parties.organizer_address) }))
            .count(),
        1,
        "the neutral projection must merge the organiser duplicate produced by Stalwart: {raw}"
    );

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
            .delete_event(&organizer_account(), None, &EventDeletion::of(&event))
            .await;
    }
    for event in jmap_events(&parties.attendee, &attendee_account())
        .await
        .into_iter()
        .filter(|e| e.uid.as_str() == parties.uid)
    {
        let _ = parties
            .attendee
            .delete_event(&attendee_account(), None, &EventDeletion::of(&event))
            .await;
    }
    for who in [&parties.organizer_auth, &parties.attendee_auth] {
        for href in inbox_hrefs_of(&parties.harness, who.auth(), &who.address, &parties.uid) {
            let _ = parties.harness.dav_delete_as(who.auth(), &href);
        }
    }
}
