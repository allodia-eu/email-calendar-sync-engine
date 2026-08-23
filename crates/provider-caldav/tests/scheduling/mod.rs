//! The live **CalDAV scheduling** scenarios (RFC 6638 auto-schedule): a real invitation
//! travelling between two real accounts on a real server.
//!
//! `calendar-semantics.md` makes a claim no offline test can reach: on a CalDAV
//! auto-schedule server, patching my `PARTSTAT` into the stored resource and `PUT`ting it
//! back **is** the whole RSVP — the server derives the iTIP `REPLY` and delivers it to the
//! organizer, so the client needs no separate delivery step. A fake cannot answer that,
//! because the reply is a thing the *server* does to a *second account's* copy in response
//! to bytes we sent. That claim is [`an_rsvp_reaches_the_organizer`], and it is why this
//! file exists.
//!
//! Stalwart is the only harness server that can run these: it advertises
//! `calendar-auto-schedule` on the calendar home and exposes
//! `CALDAV:schedule-inbox-URL`, and the harness provisions the accounts to be the two
//! parties. The SabreDAV fixture has one principal and no scheduling plugin, so — unlike
//! the write scenarios, which deliberately run against both — this module is declared only
//! by `live_caldav.rs`.
//!
//! **What the server does, observed rather than assumed.** When the organizer `PUT`s an
//! event naming an attendee, Stalwart does three separate things: deposits a
//! `METHOD:REQUEST` in the attendee's scheduling inbox, **adds the event to the attendee's
//! own calendar** already carrying `PARTSTAT=NEEDS-ACTION`, and **mails** the attendee an
//! iMIP invitation. The attendee's copy therefore appears at a **server-minted href**,
//! never at `<uid>.ics`, which is why every read here goes through a re-sync by `UID`.
//!
//! That third delivery is why both parties here are the harness's **scratch** accounts and
//! neither is the seeded one. The mail is a genuine side effect we cannot switch off, it
//! reaches the *organizer* too (a `REPLY` arrives as "Accepted:…"), and the mail suites
//! assert an exact INBOX count on the seeded account — so a run involving Alice would
//! leave her INBOX permanently over count. Off to the side, nothing counts what we deliver.
//!
//! Each scenario cleans up both calendar copies and **both parties'** scheduling-inbox
//! messages, so a re-run starts from the same state. Both, because scheduling runs both ways:
//! the attendee is sent a `REQUEST` and the organizer is sent a `REPLY`, and sweeping only the
//! attendee left the organizer's inbox growing by two per run. [`cleanup_leaves_no_residue`]
//! is the check that keeps it that way.
//!
//! **The harness has to disarm a rate limiter for this to be re-runnable.** Stalwart ships an
//! inbound throttle of 25 messages/hour per (sender domain, recipient), and auto-scheduling
//! mails *both* parties of every invitation — so this suite used to die after about four runs,
//! and it died *silently*: past the cap the server abandons the whole iTIP delivery, the
//! attendee's calendar copy included, while still answering the organizer's `PUT` with `201`.
//! The harness entrypoint now raises the inbound throttles at bootstrap
//! (`x:MtaInboundThrottle`, see `docker/stalwart/entrypoint.sh`). If these tests ever start
//! timing out on delivery again, check that first — [`poll_until`] says how.

use core::time::Duration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, Uid},
    scheduling::addresses_match,
};
use engine_provider::{EventDeletion, EventWrite, Provider};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use stalwart_harness::{Harness, ScratchAccount};

use crate::common;

/// How long a scenario waits for an asynchronous iTIP delivery to land.
///
/// The server answers the writer's `PUT` before it has finished applying the resulting
/// iTIP message to the *other* account, so the second party's copy is eventually
/// consistent. Every wait here is a **poll on the real state** ([`poll_until`]), never a
/// fixed sleep — the determinism rule in `stalwart-harness.md`.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// The two parties of a scheduling run, each with its own authenticated connection.
pub(crate) struct Parties {
    harness: Harness,
    /// The scratch account that receives the invitation (Carol).
    attendee: CalDavProvider,
    attendee_account: AccountId,
    attendee_auth: ScratchAccount,
    /// The scratch account that sends it (Bob).
    organizer: CalDavProvider,
    organizer_account: AccountId,
    organizer_auth: ScratchAccount,
}

impl Parties {
    /// The attendee's calendar address.
    fn attendee_address(&self) -> &str {
        &self.attendee_auth.address
    }

    /// The organizer's calendar address.
    pub(crate) fn organizer_address(&self) -> &str {
        &self.organizer_auth.address
    }
}

/// Connects both parties to the live harness, or `None` to skip (the offline gate).
///
/// Each side gets a provider bound to its **own** default calendar by the adapter's real
/// discovery, so the organizer writing "to my calendar" and the attendee reading "from
/// mine" are the same operation against two principals.
pub(crate) async fn parties(test: &str) -> Option<Parties> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let base = format!("http://{}", harness.http_addr);
    let connect = async |account: &ScratchAccount| {
        CalDavProvider::connect(CalDavConfig::new(
            base.clone(),
            Credentials::Basic {
                username: account.address.clone(),
                password: account.password.clone(),
            },
        ))
        .await
        .expect("connect + discover")
    };

    let [organizer_account, attendee_account] = harness.scratch.clone();
    let organizer = connect(&organizer_account).await;
    let attendee = connect(&attendee_account).await;
    Some(Parties {
        organizer_auth: organizer_account,
        attendee_auth: attendee_account,
        attendee,
        organizer,
        attendee_account: AccountId::try_from("caldav-schedule-attendee").unwrap(),
        organizer_account: AccountId::try_from("caldav-schedule-organizer").unwrap(),
        harness,
    })
}

/// How far ahead of today the invitation is scheduled. Any comfortably-future offset
/// does; two months is well clear of a slow CI queue and of a run started just before
/// midnight.
const INVITATION_DAYS_AHEAD: i64 = 60;

/// The invitation's date, as `YYYYMMDD`, a fixed offset from **today**.
///
/// The one fixture in this harness that is *not* a fixed absolute 2026 date, and
/// deliberately so. Auto-scheduling is the server deciding to deliver an iTIP message,
/// and Stalwart does not deliver one for a meeting that has already finished — so a
/// hard-coded date does not buy determinism here, it schedules a failure. The original
/// `20260810T100000` passed every run until 11:00 on 10 August 2026 and failed every run
/// after it, in eight tests at once, with a timeout that reads like a broken server.
///
/// Nothing in this module asserts an absolute instant — the scenarios assert delivery,
/// `PARTSTAT` transitions, and that the start is *zoned and resolvable* — so moving the
/// day costs no determinism the suite was actually relying on.
fn invitation_date() -> String {
    // Fully qualified: `Duration` in this module is `core::time::Duration` (the poll
    // timeout), and only `time`'s carries a calendar-aware `days`.
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(INVITATION_DAYS_AHEAD);
    format!(
        "{:04}{:02}{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// An organizer's invitation document, naming the attendee with an unanswered `RSVP`.
///
/// Assembled by hand rather than through [`engine_provider::EventDraft`] for the same
/// reason `common::write::seed` does it: a draft cannot state an `ORGANIZER`/`ATTENDEE`
/// pair, and this is the *counterparty's* fixture, not the thing under test. The times of
/// day are fixed; the day itself comes from [`invitation_date`], which explains why.
fn invitation(uid: &str, summary: &str, tzid: &str, parties: &Parties) -> String {
    let day = invitation_date();
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Harness//Scheduling//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260701T080000Z\r\nSEQUENCE:0\r\n\
         DTSTART;TZID={tzid}:{day}T100000\r\nDTEND;TZID={tzid}:{day}T110000\r\n\
         SUMMARY:{summary}\r\n\
         ORGANIZER;CN=Bob Tester:mailto:{organizer}\r\n\
         ATTENDEE;CN=Carol;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:\
         {attendee}\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        organizer = parties.organizer_address(),
        attendee = parties.attendee_address(),
    )
}

/// The organizer creates the invitation, and it is waited for on the attendee's calendar.
///
/// Returns the attendee's delivered copy — which the attendee never wrote: the server put
/// it there. That *is* [`engine_core::scheduling::SchedulingMode::ServerAutoSchedule`].
async fn invite(parties: &Parties, uid: &Uid, summary: &str, tzid: &str) -> Event {
    clean_up(parties, uid).await;
    let body = invitation(uid.as_str(), summary, tzid, parties);
    let href = parties.organizer.event_href(uid).expect("mint event href");
    parties
        .organizer
        .put_event(
            &parties.organizer_account,
            &EventWrite::unconditional(href, uid.clone(), engine_core::raw::RawIcal::new(body)),
        )
        .await
        .expect("the organizer stores the invitation");

    poll_until(
        &parties.attendee,
        &parties.attendee_account,
        uid,
        "the invitation reaches the attendee's calendar",
        |_| true,
    )
    .await
}

/// Re-reads `uid` from `provider` until `ready` accepts it, or fails after
/// [`DELIVERY_TIMEOUT`].
///
/// A poll, not a sleep: the condition is the server's actual state, so a fast server is
/// not waited on and a slow one is not raced.
async fn poll_until(
    provider: &CalDavProvider,
    account: &AccountId,
    uid: &Uid,
    what: &str,
    ready: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if let Some(event) = common::fetch(provider, account, uid.as_str()).await
            && ready(&event)
        {
            return event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {DELIVERY_TIMEOUT:?} waiting for {what}.\n\
             \n\
             Before suspecting the code: check that the harness disarmed Stalwart's inbound \
             **rate limiter**. Its default is 25 messages/hour per (sender domain, recipient), \
             auto-scheduling mails both parties of every invitation, and past the cap the \
             server abandons the *whole* iTIP delivery — the calendar copy this poll is \
             waiting for — silently, while still answering the organizer's PUT with 201. \
             `docker/stalwart/entrypoint.sh` raises it at bootstrap and logs `relaxed inbound \
             rate limiters`; look for that line in `stalwart-live.sh logs`. A re-seed \
             (`stalwart-live.sh down && up`) both resets the counters and re-applies it.\n"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Removes every trace of `uid`: the attendee's delivered copy (at its server-minted
/// href), the organizer's own copy, and the iTIP messages the server deposited in **both**
/// parties' scheduling inboxes.
///
/// RFC 6638 §3.2 makes the *client* responsible for clearing a processed scheduling-inbox
/// message, so without this the collections would grow on every run.
///
/// **Both** inboxes, because scheduling is not one-way and this fixture used to forget that.
/// The attendee receives a `REQUEST`, which was cleared — but every answered invitation also
/// deposits a `METHOD:REPLY` in the **organizer's** inbox, and that was not. The two RSVP
/// scenarios therefore left bob two replies richer per run: measured 0 → 2 → 4 over
/// consecutive runs of the suite against v0.16.15, growing without bound on a long-lived
/// harness. CI never saw it because CI always starts from `down -v`.
///
/// This is also *not* what the tracking issue (#93) predicted, which is why the check that
/// guards it ([`scenarios::cleanup_leaves_no_residue`]) asserts final state rather than a
/// call order. The predicted mechanism was a calendar-copy race — an organizer `DELETE`
/// scheduling a `CANCEL` that re-creates the attendee's already-deleted copy. That does not
/// happen: probed in isolation on both v0.16.11 and v0.16.15, a `CANCEL` addressed to a
/// resource the attendee has already removed is dropped, and neither calendar leaks.
async fn clean_up(parties: &Parties, uid: &Uid) {
    if let Some(mine) =
        common::fetch(&parties.attendee, &parties.attendee_account, uid.as_str()).await
    {
        let _ = parties
            .attendee
            .delete_event(
                &parties.attendee_account,
                None,
                &EventDeletion::unconditional(mine.id.clone(), uid.clone()),
            )
            .await;
    }
    common::pre_clean(&parties.organizer, &parties.organizer_account, uid).await;
    for who in [&parties.attendee_auth, &parties.organizer_auth] {
        for href in inbox_hrefs_of(parties, who, uid.as_str()) {
            let _ = parties.harness.dav_delete_as(who.auth(), &href);
        }
    }
}

/// The hrefs of the **attendee's** scheduling-inbox messages that carry `uid`.
fn scheduling_inbox_hrefs(parties: &Parties, uid: &str) -> Vec<String> {
    inbox_hrefs_of(parties, &parties.attendee_auth, uid)
}

/// The hrefs of the **organizer's** scheduling-inbox messages that carry `uid` — where the
/// `REPLY` to an answered invitation lands.
pub(crate) fn organizer_inbox_hrefs(parties: &Parties, uid: &str) -> Vec<String> {
    inbox_hrefs_of(parties, &parties.organizer_auth, uid)
}

/// The hrefs of `who`'s scheduling-inbox messages that carry `uid`.
///
/// Read over the harness's raw DAV helpers on purpose: exposing the RFC 6638 inbox as a
/// provider-level `REPORT` is a documented deferral (`calendar-semantics.md`), and a test
/// must not invent the feature it is meant to observe.
fn inbox_hrefs_of(parties: &Parties, who: &ScratchAccount, uid: &str) -> Vec<String> {
    let listing = parties
        .harness
        .dav_propfind_as(who.auth(), &Harness::scheduling_inbox_path_of(&who.address))
        .expect("PROPFIND the scheduling inbox");
    let body = String::from_utf8_lossy(&listing.body).into_owned();
    body.split("<D:href>")
        .skip(1)
        .filter_map(|chunk| chunk.split("</D:href>").next())
        // A Depth-1 PROPFIND lists the collection itself alongside its members; only the
        // members (the non-trailing-slash hrefs) are iTIP messages.
        .filter(|href| !href.ends_with('/'))
        .map(|href| href.replace("%40", "@"))
        // Read as the inbox's *owner* — the other party has no access, and a failed GET
        // here would silently match nothing and so delete nothing.
        .filter(|href| {
            parties
                .harness
                .dav_get_as(who.auth(), href)
                .is_ok_and(|resource| String::from_utf8_lossy(&resource.body).contains(uid))
        })
        .collect()
}

/// A cal-address without its `mailto:` scheme, for comparison against a bare address.
fn normalized(cal_address: &str) -> String {
    engine_core::scheduling::normalize_address(cal_address)
}

/// The participant whose calendar address is `address`, by the one shared comparison.
fn participant<'a>(event: &'a Event, address: &str) -> &'a engine_core::calendar::Participant {
    event
        .participants
        .iter()
        .find(|p| {
            p.email
                .as_deref()
                .is_some_and(|email| addresses_match(email, address))
        })
        .unwrap_or_else(|| panic!("a participant for {address}: {:?}", event.participants))
}

// The scenarios themselves live next door: this file is the two-party fixture (who the
// parties are, how an invitation is created, waited for, and cleaned up), and `scenarios`
// is what each one asserts. Splitting there keeps both under the 500-line limit without
// cutting across a responsibility.
pub(crate) mod reply_delivery;
pub(crate) mod scenarios;

pub(crate) use reply_delivery::an_rsvp_receipt_reports_what_the_server_said_about_delivery;
pub(crate) use scenarios::{
    an_invitation_is_delivered_to_the_attendee, an_invitations_windows_time_zone_resolves_to_iana,
    an_organizer_cancel_marks_the_attendees_copy_cancelled, an_rsvp_reaches_the_organizer,
    an_rsvp_through_the_neutral_verb_reaches_the_organizer, cleanup_leaves_no_residue,
    the_scheduling_inbox_carries_a_parseable_itip_request,
};
