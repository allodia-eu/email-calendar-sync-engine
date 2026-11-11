//! Gated live checks: calendars in a store that is not the credential's, over JMAP.
//!
//! The harness (`docker/stalwart/seed.sh`) shares three stores with alice: the `support`
//! group's mail and calendar, every right on both; bob's INBOX alone; dave's one calendar
//! alone, read-only. Every shared account *claims* mail and calendars (STALWART-1), so the
//! offline suite's captured probe answer is only as good as the request that produced it;
//! here the adapter builds and sends that request itself.
//!
//! Account ids are resolved by name, as in `live_shared.rs`: they are server-assigned.
//!
//! Skips with no `STALWART_HTTP_ADDR`.

use std::time::Duration;

use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, Uid},
    sync::{SearchDomain, SyncUpdate},
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    CalendarAddress, CalendarUserAddresses, CalendarWrites, EventDeletion, EventDraft, Invitee,
    MeetingDraft, Provider, SchedulingIdentity, SharedAccount,
};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{
    CALENDAR_SHARE_OWNER, GROUP_CALENDAR_EVENT_UID, Harness, SHARED_CALENDAR_EVENT_UID,
    SHARED_CALENDAR_NAME, SHARED_GROUP_ACCOUNT,
};

fn account() -> AccountId {
    AccountId::try_from("live-shared-calendars").unwrap()
}

fn ready(test: &str) -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

fn config(harness: &Harness, user: &str, password: &str) -> JmapConfig {
    JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(user, password),
        engine_http::test_server::user_agent(),
    )
}

async fn alice(harness: &Harness) -> JmapProvider {
    JmapProvider::connect(config(harness, &harness.account, &harness.password))
        .await
        .expect("connect")
}

/// Discovers `address` with alice's unbound client, then binds a second one to it.
async fn bound_to(harness: &Harness, address: &str) -> (SharedAccount, JmapProvider) {
    let store = alice(harness)
        .await
        .resolve_shared_account(address)
        .await
        .unwrap_or_else(|err| panic!("{address} should resolve: {err}"));
    let bound = config(harness, &harness.account, &harness.password)
        .with_shared_account(store.handle.clone());
    let provider = JmapProvider::connect(bound).await.expect("bound connect");
    (store, provider)
}

async fn calendars(provider: &JmapProvider) -> Vec<Calendar> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    objects
}

async fn events(provider: &JmapProvider, calendar: &Calendar) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(&account(), &calendar.id, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    objects
}

#[tokio::test]
async fn live_each_shared_store_is_listed_with_what_it_holds() {
    let Some(harness) = ready("live_each_shared_store_is_listed_with_what_it_holds") else {
        return;
    };
    let listed = alice(&harness)
        .await
        .list_shared_accounts()
        .await
        .expect("enumerable");
    let domains_of = |address: &str| -> Vec<SearchDomain> {
        listed
            .iter()
            .find(|store| store.address.as_deref() == Some(address))
            .unwrap_or_else(|| panic!("{address} is not listed: {listed:?}"))
            .domains
            .iter()
            .collect()
    };
    let bob = harness.read_only_share_owner().address.clone();
    assert_eq!(domains_of(&bob), [SearchDomain::Mail]);
    assert_eq!(domains_of(CALENDAR_SHARE_OWNER), [SearchDomain::Calendar]);
    assert_eq!(
        domains_of(SHARED_GROUP_ACCOUNT),
        [SearchDomain::Mail, SearchDomain::Calendar]
    );
    // Carol shares nothing, and alice's own account is not a share.
    for absent in [
        harness.scratch[1].address.as_str(),
        harness.account.as_str(),
    ] {
        assert!(
            !listed.iter().any(|s| s.address.as_deref() == Some(absent)),
            "{absent}: {listed:?}"
        );
    }
}

#[tokio::test]
async fn live_a_calendar_only_store_syncs_its_shared_calendar_read_only() {
    let Some(harness) = ready("live_a_calendar_only_store_syncs_its_shared_calendar_read_only")
    else {
        return;
    };
    let (_, dave) = bound_to(&harness, CALENDAR_SHARE_OWNER).await;
    let listed = calendars(&dave).await;
    assert_eq!(listed.len(), 1, "only the calendar dave shared: {listed:?}");
    let shared = &listed[0];
    assert_eq!(shared.name, SHARED_CALENDAR_NAME);
    assert!(
        shared.access.may_read && !shared.access.may_write,
        "{shared:?}"
    );
    assert!(!shared.access.may_rsvp && !shared.access.may_delete);
    let uids: Vec<String> = events(&dave, shared)
        .await
        .iter()
        .map(|e| e.uid.as_str().to_owned())
        .collect();
    assert_eq!(uids, [SHARED_CALENDAR_EVENT_UID]);
}

#[tokio::test]
async fn live_the_group_store_syncs_its_own_calendar_writable() {
    let Some(harness) = ready("live_the_group_store_syncs_its_own_calendar_writable") else {
        return;
    };
    let (_, group) = bound_to(&harness, SHARED_GROUP_ACCOUNT).await;
    let listed = calendars(&group).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(listed[0].access.may_write && listed[0].access.may_rsvp);
    let held = events(&group, &listed[0]).await;
    assert!(
        held.iter()
            .any(|e| e.uid.as_str() == GROUP_CALENDAR_EVENT_UID),
        "{held:?}"
    );
    // None of alice's own seed is in the group's calendar.
    assert!(
        !held
            .iter()
            .any(|e| e.uid.as_str() == "oneoff-2001@test.local")
    );
}

/// The bound-store rule (`engine_provider::calendar_user`), in both directions: the group,
/// in which alice schedules as the group, answers with the group's address alone; dave's
/// read-only share, in which she schedules as nobody, answers nobody. Unbound, her own
/// account still answers with both of her identities (`live_calendar_user.rs`).
#[tokio::test]
async fn live_a_bound_store_answers_for_its_owner_and_never_for_the_credential() {
    let Some(harness) =
        ready("live_a_bound_store_answers_for_its_owner_and_never_for_the_credential")
    else {
        return;
    };
    let (_, group) = bound_to(&harness, SHARED_GROUP_ACCOUNT).await;
    let answer = group.calendar_user_addresses(&account()).await.unwrap();
    assert_eq!(
        answer,
        CalendarUserAddresses::Known(vec![SHARED_GROUP_ACCOUNT.to_owned()])
    );
    assert!(answer.is_for_store(SHARED_GROUP_ACCOUNT, &harness.account));

    let (_, dave) = bound_to(&harness, CALENDAR_SHARE_OWNER).await;
    let answer = dave.calendar_user_addresses(&account()).await.unwrap();
    assert_eq!(answer, CalendarUserAddresses::NotEnabled);
    assert!(answer.is_for_store(CALENDAR_SHARE_OWNER, &harness.account));
}

const MEETING_UID: &str = "jmap-shared-meeting@test.local";

/// A meeting made in the group's calendar is organised **as the group**, and the server
/// schedules it: the attendee's copy arrives naming the group as organiser.
///
/// Carol is the attendee because she is a scratch account (`stalwart-harness.md`): the
/// invitation the server mails her lands in no folder any suite counts. The date is a fixed
/// offset from today, because Stalwart does not schedule a meeting that has already ended.
#[tokio::test]
async fn live_a_meeting_made_in_the_group_calendar_is_organised_by_the_group() {
    let Some(harness) =
        ready("live_a_meeting_made_in_the_group_calendar_is_organised_by_the_group")
    else {
        return;
    };
    let (_, group) = bound_to(&harness, SHARED_GROUP_ACCOUNT).await;
    let carol_auth = harness.scratch[1].clone();
    let carol = JmapProvider::connect(config(&harness, &carol_auth.address, &carol_auth.password))
        .await
        .expect("carol connects");
    clean_up(&harness, &group, &carol).await;

    let calendar = calendars(&group).await.remove(0);
    let draft = EventDraft::new(
        calendar.id.clone(),
        Uid::new(MEETING_UID).unwrap(),
        "Support handover",
        in_sixty_days(14),
        in_sixty_days(15),
        UtcDateTime::new(2026, 7, 1, 8, 0, 0).unwrap(),
    )
    .meeting(MeetingDraft::new(
        SchedulingIdentity::named(
            CalendarAddress::parse(SHARED_GROUP_ACCOUNT).unwrap(),
            "Support",
        ),
        vec![Invitee::required(SchedulingIdentity::named(
            CalendarAddress::parse(&carol_auth.address).unwrap(),
            "Carol",
        ))],
    ));
    group
        .create_event(&account(), &draft)
        .await
        .expect("alice creates the meeting in the group calendar");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let delivered = loop {
        if let Some(event) = carol_copy(&carol).await {
            break event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the invitation never reached carol"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let raw: serde_json::Value =
        serde_json::from_str(delivered.raw_jscalendar.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(
        raw["organizerCalendarAddress"],
        format!("mailto:{SHARED_GROUP_ACCOUNT}"),
        "{raw}"
    );
    clean_up(&harness, &group, &carol).await;
}

fn in_sixty_days(hour: u8) -> CalendarDateTime {
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(60);
    let local = format!(
        "{:04}-{:02}-{:02}T{hour:02}:00:00",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
    .parse()
    .unwrap();
    CalendarDateTime::Zoned {
        local,
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

async fn carol_copy(carol: &JmapProvider) -> Option<Event> {
    for calendar in calendars(carol).await {
        if let Some(event) = events(carol, &calendar)
            .await
            .into_iter()
            .find(|e| e.uid.as_str() == MEETING_UID)
        {
            return Some(event);
        }
    }
    None
}

/// Removes the meeting from both calendars, and carol's iTIP messages from her scheduling
/// inbox, which RFC 6638 §3.2 leaves to the client. Every run leaves the harness as it found it.
async fn clean_up(harness: &Harness, group: &JmapProvider, carol: &JmapProvider) {
    for provider in [group, carol] {
        for calendar in calendars(provider).await {
            for event in events(provider, &calendar)
                .await
                .into_iter()
                .filter(|e| e.uid.as_str() == MEETING_UID)
            {
                let _ = provider
                    .delete_event(&account(), None, &EventDeletion::of(&event))
                    .await;
            }
        }
    }
    let carol_auth = &harness.scratch[1];
    let inbox = Harness::scheduling_inbox_path_of(&carol_auth.address);
    let Ok(listing) = harness.dav_propfind_as(carol_auth.auth(), &inbox) else {
        return;
    };
    let body = String::from_utf8_lossy(&listing.body).into_owned();
    for href in body
        .split("<D:href>")
        .skip(1)
        .filter_map(|chunk| chunk.split("</D:href>").next())
        .filter(|href| !href.ends_with('/'))
    {
        let mine = harness
            .dav_get_as(carol_auth.auth(), href)
            .is_ok_and(|r| String::from_utf8_lossy(&r.body).contains(MEETING_UID));
        if mine {
            let _ = harness.dav_delete_as(carol_auth.auth(), href);
        }
    }
}
