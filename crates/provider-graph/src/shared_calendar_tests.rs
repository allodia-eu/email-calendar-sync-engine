//! The shared-*calendar* probe: the calendar provider's half of shared-store discovery.
//!
//! Same contract as the mail probe (`shared_tests.rs`), on the route a calendar sync
//! depends on: `GET /users/{address}/calendar?$select=id`. Every body is a capture from a real
//! tenant, scrubbed per `tests/fixtures/README.md`.

use engine_core::{error::FailureClass, sync::SearchDomain};
use engine_provider::{Provider, SharedAccounts, StoreDomains};

use super::*;
use crate::{
    GraphCalendarProvider,
    cal_fetch::CalendarWindow,
    test_support::{FakeRoute, fake_client_fallible, json},
};

const PROBE_OK: &str = include_str!("../tests/fixtures/calendar/shared_calendar_probe.json");
const OWN: &str = include_str!("../tests/fixtures/calendar/own_calendar_probe.json");
const INVALID_USER: &str =
    include_str!("../tests/fixtures/error/shared_calendar_invalid_user.json");
const NO_CALENDAR: &str = include_str!("../tests/fixtures/error/shared_calendar_no_calendar.json");
const ACCESS_DENIED: &str =
    include_str!("../tests/fixtures/error/shared_mailbox_access_denied.json");

const SHARED: &str = "shared@example.test";
const SHARED_ROUTE: &str = "/users/shared@example.test/calendar?$select=id";
const OWN_ROUTE: &str = "/me/calendar?$select=id";

fn client(shared: FakeRoute, own: FakeRoute) -> GraphClient {
    fake_client_fallible(vec![(SHARED_ROUTE, shared), (OWN_ROUTE, own)])
}

async fn class_when_probe_answers(status: u16, body: &str) -> FailureClass {
    let client = client(Err((status, json(body))), Ok(json(OWN)));
    resolve_calendar(&client, SHARED).await.unwrap_err().class()
}

#[tokio::test]
async fn a_calendar_the_credential_can_open_resolves_as_a_calendar_store() {
    let client = client(Ok(json(PROBE_OK)), Ok(json(OWN)));
    let resolved = resolve_calendar(&client, SHARED).await.unwrap();
    assert_eq!(resolved.handle.as_str(), SHARED);
    assert_eq!(resolved.address.as_deref(), Some(SHARED));
    assert_eq!(resolved.domains, StoreDomains::of(SearchDomain::Calendar));
}

#[tokio::test]
async fn the_credentials_own_calendar_is_refused_under_any_address() {
    // Observed: the default calendar's id differs between `/me/calendar` and the shared
    // mailbox's, and is the same through `/me` and `/users/{own address}`.
    let own_again = json(&PROBE_OK.replace("calendar-shared-default", "calendar-own-default"));
    let err = resolve_calendar(&client(Ok(own_again), Ok(json(OWN))), SHARED)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.detail().contains("own"), "{err}");
}

#[tokio::test]
async fn every_404_is_not_resolvable_and_a_403_is_a_grant_shortfall() {
    assert_eq!(
        class_when_probe_answers(404, INVALID_USER).await,
        FailureClass::Permanent
    );
    // A principal with no calendar at all (captured: a group address).
    assert_eq!(
        class_when_probe_answers(404, NO_CALENDAR).await,
        FailureClass::Permanent
    );
    assert_eq!(
        class_when_probe_answers(403, ACCESS_DENIED).await,
        FailureClass::Authentication
    );
}

#[tokio::test]
async fn an_address_that_cannot_go_in_a_url_never_reaches_the_tenant() {
    let client = client(Ok(json(PROBE_OK)), Ok(json(OWN)));
    let err = resolve_calendar(&client, "shared@example.test/../me")
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn the_calendar_provider_resolves_by_address_and_cannot_list() {
    let provider = GraphCalendarProvider::new(
        client(Ok(json(PROBE_OK)), Ok(json(OWN))),
        CalendarWindow::new(
            engine_core::time::CalendarDate::new(2026, 1, 1).unwrap(),
            engine_core::time::CalendarDate::new(2027, 1, 1).unwrap(),
        ),
        engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    );
    assert_eq!(
        provider.connection_info().capabilities.shared_accounts(),
        SharedAccounts::ByAddress
    );
    assert_eq!(
        provider.list_shared_accounts().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let resolved = provider.resolve_shared_account(SHARED).await.unwrap();
    assert_eq!(resolved.domains, StoreDomains::of(SearchDomain::Calendar));
}

/// The bound-store rule's two halves under `/users/{shared}`: the shared mailbox is the
/// calendar user where the delegate may write its calendar, and nobody where they may only read
/// it (`engine_provider::calendar_user`). `canEdit` is the shape the default calendar answered
/// live; a read-only delegate is not on the test tenant, so its `false` is the documented field
/// rather than a capture.
mod bound_calendar_user {
    use engine_provider::{CalendarUserAddresses, CalendarWrites};
    use serde_json::json as value;

    use super::*;

    async fn answer(can_edit: bool) -> (CalendarUserAddresses, bool) {
        let routes = vec![
            (
                "/users/shared@example.test?$select=displayName,mail,userPrincipalName",
                Ok(value!({ "displayName": "Shared", "mail": SHARED,
                    "userPrincipalName": SHARED })),
            ),
            (
                "/users/shared@example.test/calendar?$select=canEdit",
                Ok(value!({ "canEdit": can_edit })),
            ),
        ];
        let client =
            fake_client_fallible(routes).with_principal(MailboxPrincipal::user(SHARED).unwrap());
        let provider = GraphCalendarProvider::new(
            client,
            CalendarWindow::new(
                engine_core::time::CalendarDate::new(2026, 1, 1).unwrap(),
                engine_core::time::CalendarDate::new(2027, 1, 1).unwrap(),
            ),
            engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        );
        let account = engine_core::ids::AccountId::try_from("bound").unwrap();
        let got = provider.calendar_user_addresses(&account).await.unwrap();
        let ok = got.is_for_store(SHARED, "delegate@example.test");
        (got, ok)
    }

    #[tokio::test]
    async fn a_calendar_the_delegate_may_edit_schedules_as_the_shared_mailbox() {
        let (got, ok) = answer(true).await;
        assert_eq!(
            got,
            CalendarUserAddresses::Including(vec![SHARED.to_owned()])
        );
        assert!(ok);
    }

    #[tokio::test]
    async fn a_calendar_the_delegate_may_only_read_schedules_as_nobody() {
        let (got, ok) = answer(false).await;
        assert_eq!(got, CalendarUserAddresses::NotEnabled);
        assert!(ok);
    }
}
