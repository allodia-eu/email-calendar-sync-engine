//! A new event's id asked through `dyn Provider`, the way a host that holds a boxed adapter
//! asks it: the same answer the adapter's own creates and imports use.

use std::sync::Arc;

use engine_core::ids::{CalendarId, Uid};
use engine_provider::{IgnoreConnectSteps, Provider};

use crate::{
    CalDavProvider,
    test_support::{Replay, ok, options},
};

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");

async fn boxed() -> Box<dyn Provider> {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, calendar-access")),
    ]));
    let provider =
        CalDavProvider::with_executor(Box::new(replay), "/.well-known/caldav", &IgnoreConnectSteps)
            .await
            .expect("discovery");
    Box::new(provider)
}

fn default_calendar() -> CalendarId {
    CalendarId::try_from("/dav/cal/alice%40test.local/default/").unwrap()
}

#[tokio::test]
async fn a_new_event_is_named_by_its_uid_with_the_at_sign_kept() {
    let provider = boxed().await;
    // Kept as `@`, the form Stalwart, SabreDAV and Nextcloud list the object under, so the id
    // a host reads back by is the id the server reports.
    let minted = provider
        .new_event_id(
            &default_calendar(),
            &Uid::new("meeting-9@test.local").unwrap(),
        )
        .unwrap();
    assert_eq!(
        minted.as_str(),
        "/dav/cal/alice%40test.local/default/meeting-9@test.local.ics"
    );
}

#[tokio::test]
async fn a_uid_too_long_to_be_a_resource_name_is_named_by_its_hash() {
    let provider = boxed().await;
    let uid = Uid::new("0".repeat(300)).unwrap();
    let minted = provider.new_event_id(&default_calendar(), &uid).unwrap();
    let name = minted
        .as_str()
        .strip_prefix("/dav/cal/alice%40test.local/default/")
        .and_then(|rest| rest.strip_suffix(".ics"))
        .expect("a resource in the calendar it was given");
    assert_eq!(name, crate::href::hashed_name(uid.as_str().as_bytes()));
}

#[tokio::test]
async fn a_calendar_named_by_anything_but_its_href_is_refused() {
    let provider = boxed().await;
    let err = provider
        .new_event_id(
            &CalendarId::try_from("work").unwrap(),
            &Uid::new("x@test.local").unwrap(),
        )
        .unwrap_err();
    assert_eq!(err.class(), engine_core::error::FailureClass::Permanent);
}
