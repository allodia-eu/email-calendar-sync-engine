//! The one judgement of a refused write, and the sync create reading it as the import does.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, CalendarId, Uid},
    time::CalendarDateTime,
};
use engine_provider::{CalendarWrites, EventDraft, IgnoreConnectSteps, TargetRefusal};

use super::WriteRefusal;
use crate::{
    CalDavProvider,
    error::CalDavError,
    test_support::{Replay, ok, options, status},
};

const STALWART_INVALID: &str = include_str!("../tests/fixtures/import/stalwart-put-invalid.xml");
const NEXTCLOUD_INVALID: &str = include_str!("../tests/fixtures/import/nextcloud-put-invalid.xml");
const NEXTCLOUD_HELD: &str = include_str!("../tests/fixtures/import/nextcloud-put-second-uid.xml");
const THROTTLED: &str = include_str!("../tests/fixtures/concurrency-limit-stalwart.json");
const NEED_PRIVILEGES: &str = "<d:error xmlns:d=\"DAV:\"><d:need-privileges><d:resource><d:href>/c/</d:href>\
     <d:privilege><d:bind/></d:privilege></d:resource></d:need-privileges></d:error>";

fn judged(code: u16, body: &str) -> Option<WriteRefusal> {
    CalDavError::status(code, body).write_refusal()
}

#[test]
fn each_answer_is_judged_once() {
    let target = WriteRefusal::Target;
    for (code, body, expected) in [
        (412, STALWART_INVALID, Some(WriteRefusal::Object)),
        (412, "", None),
        (403, NEXTCLOUD_INVALID, Some(WriteRefusal::Object)),
        (403, NEXTCLOUD_HELD, None),
        (409, NEXTCLOUD_HELD, None),
        (
            403,
            NEED_PRIVILEGES,
            Some(target(TargetRefusal::ReadOnlyContainer)),
        ),
        (403, "", Some(target(TargetRefusal::ReadOnlyContainer))),
        (404, "", Some(target(TargetRefusal::NoSuchContainer))),
        (409, "", Some(target(TargetRefusal::NoSuchContainer))),
        (507, "", Some(target(TargetRefusal::StorageFull))),
        (400, THROTTLED, None),
        (400, "", Some(WriteRefusal::Object)),
        (413, "", Some(WriteRefusal::Object)),
        (415, "", Some(WriteRefusal::Object)),
        (500, "", None),
    ] {
        assert_eq!(judged(code, body), expected, "{code} {body}");
    }
    assert_eq!(CalDavError::xml("x").write_refusal(), None);
}

async fn create_answered(answer: crate::response::HttpResponse) -> engine_provider::ProviderError {
    let replay = Arc::new(Replay::new(vec![
        ok(include_str!("../tests/fixtures/principal.xml")),
        options(Some("1, 3, calendar-access")),
        answer,
    ]));
    let provider = CalDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let start = CalendarDateTime::Floating("2026-06-22T09:00:00".parse().unwrap());
    let draft = EventDraft::new(
        CalendarId::try_from("/dav/cal/alice%40test.local/work/").unwrap(),
        Uid::new("new@test.local").unwrap(),
        "Planning",
        start.clone(),
        start,
        "2026-06-20T00:00:00Z".parse().unwrap(),
    );
    provider
        .create_event(&AccountId::try_from("a").unwrap(), &draft)
        .await
        .expect_err("refused")
}

#[tokio::test]
async fn a_sync_create_reads_a_refusal_as_an_import_does() {
    let invalid = create_answered(status(412, STALWART_INVALID)).await;
    assert_eq!(invalid.class(), FailureClass::Permanent, "not a Conflict");
    assert!(!invalid.detail().contains("valid-calendar-object-resource"));

    let read_only = create_answered(status(403, NEED_PRIVILEGES)).await;
    assert_eq!(
        read_only.target_refusal(),
        Some(TargetRefusal::ReadOnlyContainer)
    );

    let taken = create_answered(status(412, "")).await;
    assert_eq!(taken.class(), FailureClass::Conflict);
    assert_eq!(taken.target_refusal(), None);
}
