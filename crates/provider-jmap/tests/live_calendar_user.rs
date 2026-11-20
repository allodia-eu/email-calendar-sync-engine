//! Gated live integration: the calendar user addresses the Stalwart harness recognises over
//! JMAP, read from its `ParticipantIdentity` objects.
//!
//! The offline suite serves a captured response whatever it is sent, so a request naming the
//! wrong capability or account passes there and fails here.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use engine_core::ids::AccountId;
use engine_provider::{CalendarUserAddresses, CalendarWrites};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT};

async fn addresses_of(harness: &Harness, user: &str, password: &str) -> CalendarUserAddresses {
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(user, password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect");
    provider
        .calendar_user_addresses(&AccountId::try_from("live").unwrap())
        .await
        .expect("ParticipantIdentity/get")
}

/// Stalwart gives a member of a group a participant identity for the group's address too:
/// alice is in the harness's `support` group, and bob, in no group, is the control that shows
/// the second address comes from that membership and not from anything the adapter reads
/// wrongly. The CalDAV principal's address set names alice alone on the same server
/// (`provider-caldav`'s `live_calendar_user`).
#[tokio::test]
async fn the_participant_identities_name_the_accounts_own_address_and_its_groups() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live calendar user test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");

    let alice = addresses_of(&harness, &harness.account, &harness.password).await;
    assert_eq!(
        alice,
        CalendarUserAddresses::Known(vec![
            harness.account.clone(),
            SHARED_GROUP_ACCOUNT.to_owned()
        ])
    );

    let bob = harness.read_only_share_owner();
    assert_eq!(
        addresses_of(&harness, &bob.address, &bob.password).await,
        CalendarUserAddresses::Known(vec![bob.address.clone()])
    );
}
