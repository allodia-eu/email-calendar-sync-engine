//! A provider bound to a shared store's principal (`CalDavConfig::with_shared_account`), over
//! the captured answers of the Stalwart harness for dave's calendar-only share.

use std::sync::Arc;

use engine_core::{ids::AccountId, sync::SyncUpdate};
use engine_provider::{
    CalendarUserAddresses, CalendarWrites, IgnoreConnectSteps, Provider, SharedAccounts,
};

use crate::{
    provider::CalDavProvider,
    test_support::{Replay, ok, options},
};

const ORIGIN: &str = "http://127.0.0.1:18180";

fn fixture(name: &str) -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{dir}/tests/fixtures/{name}")).expect("fixture")
}

fn account() -> AccountId {
    AccountId::try_from("bound").unwrap()
}

async fn bound_to_dave(more: Vec<crate::transport::HttpResponse>) -> (CalDavProvider, Arc<Replay>) {
    let mut responses = vec![
        ok(&fixture("shared-principal-stalwart-dave.xml")),
        options(Some(include_str!(
            "../tests/fixtures/options-dav-stalwart.txt"
        ))),
    ];
    responses.extend(more);
    let exec = Arc::new(Replay::new(responses));
    let provider = CalDavProvider::with_executor_bound(
        Box::new(exec.clone()),
        &format!("{ORIGIN}/dav/pal/dave%40test.local/"),
        &IgnoreConnectSteps,
    )
    .await
    .expect("bound connect");
    (provider, exec)
}

#[tokio::test]
async fn a_bound_store_the_user_may_write_answers_as_its_owner() {
    // A home listing a calendar the user may write (captured: the support group's).
    let (provider, exec) =
        bound_to_dave(vec![ok(&fixture("shared-home-stalwart-support.xml"))]).await;
    // Discovery asked the bound principal, not the well-known or alice's.
    assert_eq!(
        exec.seen()[0].1,
        format!("{ORIGIN}/dav/pal/dave%40test.local/")
    );
    // The store's calendar user is its owner, never the credential.
    let answer = provider.calendar_user_addresses(&account()).await.unwrap();
    assert_eq!(
        answer,
        CalendarUserAddresses::Known(vec!["dave@test.local".to_owned()])
    );
    assert!(answer.is_for_store("dave@test.local", "alice@test.local"));
    // And it can still find the stores the credential reaches.
    assert_eq!(
        provider.connection_info().capabilities.shared_accounts(),
        SharedAccounts::Enumerable
    );
}

/// The rule's other half: in a store the user may only read, they schedule as nobody, which
/// is what JMAP answers for the same store. Dave's real home is read-only for alice.
#[tokio::test]
async fn a_bound_store_the_user_may_only_read_answers_nobody() {
    let (provider, _) = bound_to_dave(vec![ok(&fixture("shared-home-stalwart-dave.xml"))]).await;
    assert_eq!(
        provider.calendar_user_addresses(&account()).await.unwrap(),
        CalendarUserAddresses::NotEnabled
    );
}

#[tokio::test]
async fn a_calendar_in_the_bound_store_names_its_owner_and_its_rights() {
    let (provider, _) = bound_to_dave(vec![
        ok(&fixture("shared-home-stalwart-dave.xml")),
        ok(&fixture("shared-principal-stalwart-dave.xml")),
    ])
    .await;
    let listing = provider.sync_calendars(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = listing.sync.update else {
        panic!("a snapshot");
    };
    assert_eq!(objects.len(), 1, "{objects:?}");
    assert_eq!(objects[0].name, "Dave Shared");
    assert_eq!(objects[0].owner.as_deref(), Some("dave@test.local"));
    assert!(!objects[0].access.may_write);
}

#[tokio::test]
async fn listing_from_a_bound_provider_starts_at_the_credentials_principal() {
    let (provider, exec) = bound_to_dave(vec![crate::test_support::status(404, "")]).await;
    assert_eq!(provider.list_shared_accounts().await.unwrap(), []);
    assert_eq!(
        exec.seen().last().unwrap().1,
        format!("{ORIGIN}/dav/pal/alice%40test.local/")
    );
}

/// Why binding is not simply "start discovery at the shared principal": discovery's shortcut
/// keeps the address set only when the resource it asked reported one, and otherwise asks the
/// `current-user-principal` later, which is the credential's. A shared principal that reports
/// no set would then answer with the user's own addresses, the one thing the bound-store rule
/// forbids. Bound discovery reads the store's principal alone: no set is `NotEnabled`.
#[tokio::test]
async fn a_bound_principal_without_addresses_never_answers_with_the_users() {
    let without = fixture("shared-principal-stalwart-dave.xml").replace(
        "<A:calendar-user-address-set><D:href>mailto:dave@test.local</D:href></A:calendar-user-address-set>",
        "",
    );
    let exec = Arc::new(Replay::new(vec![
        ok(&without),
        options(Some(include_str!(
            "../tests/fixtures/options-dav-stalwart.txt"
        ))),
    ]));
    let dave = format!("{ORIGIN}/dav/pal/dave%40test.local/");
    let provider =
        CalDavProvider::with_executor_bound(Box::new(exec.clone()), &dave, &IgnoreConnectSteps)
            .await
            .unwrap();
    assert_eq!(
        provider.calendar_user_addresses(&account()).await.unwrap(),
        CalendarUserAddresses::NotEnabled
    );
    // Nothing more was asked: in particular not alice's principal.
    assert_eq!(exec.seen().len(), 2);
    // And the principal a host would resume from is the store's, not the credential's.
    assert_eq!(provider.principal_url(), Some(dave.as_str()));
}

/// A share since withdrawn fails the connect, not every call after it: the principal names no
/// home the credential may read.
#[tokio::test]
async fn a_shared_principal_with_no_home_fails_the_connect() {
    let without = fixture("shared-principal-stalwart-dave.xml").replace(
        "<A:calendar-home-set><D:href>/dav/cal/dave%40test.local/</D:href></A:calendar-home-set>",
        "",
    );
    let exec = Replay::new(vec![ok(&without)]);
    let err = CalDavProvider::with_executor_bound(
        Box::new(exec),
        &format!("{ORIGIN}/dav/pal/dave%40test.local/"),
        &IgnoreConnectSteps,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("lost access"), "{err}");
}
