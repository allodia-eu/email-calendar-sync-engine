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
use stalwart_harness::Harness;

#[tokio::test]
async fn the_participant_identities_name_the_accounts_own_address() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live calendar user test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    let addresses = provider
        .calendar_user_addresses(&AccountId::try_from("live").unwrap())
        .await
        .expect("ParticipantIdentity/get");
    assert_eq!(
        addresses,
        CalendarUserAddresses::Known(vec![harness.account.clone()])
    );
}
