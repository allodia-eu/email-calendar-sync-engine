//! Gated live checks: other principals' calendar homes, found and opened over CalDAV, on both
//! harness servers that can share one.
//!
//! The servers reach the same stores by different routes (`provider-caldav/src/shared.rs`):
//! Stalwart lists them in its principal collection and refuses the homes alice may not read;
//! SabreDAV names them in its proxy sets and empties, rather than refuses, the homes she may
//! not read. One assertion runs against each.
//!
//! Skips per server: `STALWART_HTTP_ADDR` for Stalwart, `SABREDAV_HTTP_ADDR` for SabreDAV.

use core::time::Duration;

use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, SharedAccountId},
    sync::{SearchDomain, SyncUpdate},
};
use engine_provider::{
    CalendarUserAddresses, CalendarWrites, Provider, SharedAccount, SharedAccounts,
};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use stalwart_harness::{
    CALENDAR_SHARE_OWNER, GROUP_CALENDAR_EVENT_UID, Harness, SHARED_CALENDAR_EVENT_UID,
    SHARED_CALENDAR_NAME, SHARED_GROUP_ACCOUNT,
};

const USER: &str = "alice@test.local";

fn account() -> AccountId {
    AccountId::try_from("live-caldav-shared").unwrap()
}

/// One server: its base URL and alice's password there.
struct Server {
    base: String,
    password: String,
}

fn stalwart() -> Option<Server> {
    let harness = Harness::from_env()?;
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("ready");
    Some(Server {
        base: format!("http://{}", harness.http_addr),
        password: harness.password,
    })
}

fn sabredav() -> Option<Server> {
    Some(Server {
        base: format!("http://{}", std::env::var("SABREDAV_HTTP_ADDR").ok()?),
        password: "sabredav-alice-pw".to_owned(),
    })
}

impl Server {
    fn config(&self) -> CalDavConfig {
        CalDavConfig::new(
            self.base.clone(),
            Credentials::Basic {
                username: USER.to_owned(),
                password: self.password.clone(),
            },
            engine_http::test_server::user_agent(),
        )
    }

    async fn own(&self) -> CalDavProvider {
        CalDavProvider::connect(self.config())
            .await
            .expect("connect")
    }

    async fn bound(&self, handle: &SharedAccountId) -> CalDavProvider {
        CalDavProvider::connect(self.config().with_shared_account(handle.clone()))
            .await
            .expect("bound connect")
    }
}

async fn calendars(provider: &CalDavProvider) -> Vec<Calendar> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync
        .update
    else {
        panic!("a snapshot");
    };
    objects
}

async fn events(provider: &CalDavProvider, calendar: &Calendar) -> Vec<Event> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_events(&account(), &calendar.id, None)
        .await
        .expect("sync events")
        .update
    else {
        panic!("a snapshot");
    };
    objects
}

fn store<'a>(listed: &'a [SharedAccount], address: &str) -> &'a SharedAccount {
    listed
        .iter()
        .find(|s| s.address.as_deref() == Some(address))
        .unwrap_or_else(|| panic!("{address} is not listed: {listed:?}"))
}

/// What both servers must agree on: dave's and the group's stores are listed, as calendars,
/// and nobody else's; dave's opens read-only on his one calendar, the group's writable on its
/// own, each naming its owner and answering for that owner alone.
async fn the_shared_stores_are_found_and_open_as_their_owners(server: Server) {
    let own = server.own().await;
    assert_eq!(
        own.connection_info().capabilities.shared_accounts(),
        SharedAccounts::Enumerable
    );
    let listed = own.list_shared_accounts().await.expect("listing");
    assert_eq!(listed.len(), 2, "dave and the group alone: {listed:?}");
    for found in &listed {
        assert_eq!(
            found.domains.iter().collect::<Vec<_>>(),
            [SearchDomain::Calendar]
        );
    }

    for (owner, uid, writable) in [
        (CALENDAR_SHARE_OWNER, SHARED_CALENDAR_EVENT_UID, false),
        (SHARED_GROUP_ACCOUNT, GROUP_CALENDAR_EVENT_UID, true),
    ] {
        let bound = server.bound(&store(&listed, owner).handle).await;
        let held = calendars(&bound).await;
        assert_eq!(held.len(), 1, "{owner}: {held:?}");
        assert_eq!(held[0].owner.as_deref(), Some(owner));
        assert_eq!(held[0].access.may_write, writable, "{owner}: {held:?}");
        let uids: Vec<String> = events(&bound, &held[0])
            .await
            .iter()
            .map(|e| e.uid.as_str().to_owned())
            .collect();
        assert_eq!(uids, [uid], "{owner}");

        let answer = bound.calendar_user_addresses(&account()).await.unwrap();
        assert!(answer.is_for_store(owner, USER), "{owner}: {answer:?}");
        // Where alice may write, she schedules as the owner; where she may only read, as
        // nobody — the same answer JMAP gives for dave's store.
        if writable {
            assert!(answer.recognises(owner), "{owner}: {answer:?}");
        } else {
            assert_eq!(answer, CalendarUserAddresses::NotEnabled, "{owner}");
        }
        // A bound provider lists the same stores: what a credential reaches is its own fact.
        assert_eq!(bound.list_shared_accounts().await.unwrap(), listed);
    }
    assert_eq!(
        store(&listed, CALENDAR_SHARE_OWNER).name,
        "Dave Tester",
        "{listed:?}"
    );

    // Alice's own home: what lives in it has no owner, whatever the server says of an in-home
    // share (SabreDAV names the sharee, alice, as `bob-readonly`'s). Stalwart also lists the
    // calendars shared with her there, each a collection of its owner's home, and each says
    // whose it is.
    for calendar in calendars(&own).await {
        let id = calendar.id.as_str().replace("%40", "@");
        match [CALENDAR_SHARE_OWNER, SHARED_GROUP_ACCOUNT]
            .into_iter()
            .find(|owner| id.contains(&format!("/{owner}/")))
        {
            Some(owner) => assert_eq!(calendar.owner.as_deref(), Some(owner), "{calendar:?}"),
            None => assert_eq!(calendar.owner, None, "{calendar:?}"),
        }
    }
}

#[tokio::test]
async fn stalwart_shared_calendar_stores() {
    let Some(server) = stalwart() else {
        eprintln!("skipping stalwart_shared_calendar_stores: STALWART_HTTP_ADDR unset");
        return;
    };
    let own = server.own().await;
    let dave = own.list_shared_accounts().await.unwrap();
    // Stalwart's dave holds one calendar alone, named as the seed names it.
    let bound = server
        .bound(&store(&dave, CALENDAR_SHARE_OWNER).handle)
        .await;
    assert_eq!(calendars(&bound).await[0].name, SHARED_CALENDAR_NAME);
    // Stalwart also lists the calendars shared with alice in her own home, so its harness
    // proves the read-only half of `DAV:current-user-privilege-set` too: dave's, seen there.
    let in_own_home = calendars(&own)
        .await
        .into_iter()
        .find(|c| c.name == SHARED_CALENDAR_NAME)
        .expect("dave's calendar is listed in alice's own home");
    assert!(in_own_home.access.may_read && !in_own_home.access.may_write);
    the_shared_stores_are_found_and_open_as_their_owners(server).await;
}

#[tokio::test]
async fn sabredav_shared_calendar_stores() {
    let Some(server) = sabredav() else {
        eprintln!("skipping sabredav_shared_calendar_stores: SABREDAV_HTTP_ADDR unset");
        return;
    };
    the_shared_stores_are_found_and_open_as_their_owners(server).await;
}
