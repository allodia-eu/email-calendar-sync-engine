//! Gated live integration: the calendar user addresses a real CalDAV server recognises.
//!
//! Stalwart names the calendar home at its well-known target but answers the address set
//! `404` there; only the principal carries it. So this proves the provider reaches the
//! principal rather than reading the first response it got, which the offline suite can only
//! assert against a transcript of today's Stalwart.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use core::time::Duration;

use engine_core::ids::AccountId;
use engine_provider::{CalendarUserAddresses, CalendarWrites};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use stalwart_harness::Harness;

#[tokio::test]
async fn stalwart_recognises_the_accounts_own_address() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping stalwart_recognises_the_accounts_own_address: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let provider = CalDavProvider::connect(CalDavConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("connect + discover");
    let account = AccountId::try_from("caldav-calendar-user-live").unwrap();

    let addresses = provider
        .calendar_user_addresses(&account)
        .await
        .expect("read the address set");
    assert_eq!(
        addresses,
        CalendarUserAddresses::Known(vec![harness.account.clone()]),
        "the principal's calendar-user-address-set names the account's own address"
    );
    assert!(!addresses.recognises("bob@test.local"));
}
