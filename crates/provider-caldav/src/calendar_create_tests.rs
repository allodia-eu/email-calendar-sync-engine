//! `MKCALENDAR` and the read-back, over the answers Stalwart gave to the same requests
//! (`tests/fixtures/import/`). Proven live in `tests/common/import.rs`.

use std::sync::Arc;

use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::{CalendarCollections, IgnoreConnectSteps, NewCalendar, ProviderResult};

use crate::{
    CalDavProvider,
    response::HttpResponse,
    test_support::{Replay, ok, options, status, wrote},
    transport::{DavMethod, Precondition},
};

const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");
const CREATED: &str = include_str!("../tests/fixtures/import/stalwart-created-listing.xml");

async fn create(
    after: Vec<HttpResponse>,
) -> (ProviderResult<engine_core::calendar::Calendar>, Arc<Replay>) {
    let mut responses = vec![ok(PRINCIPAL), options(Some("1, 3, calendar-access"))];
    responses.extend(after);
    let replay = Arc::new(Replay::new(responses));
    let provider = CalDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    let new = NewCalendar::new("Imported & Kept")
        .with_description("From the source")
        .with_color("#FF8800FF");
    let made = provider
        .create_calendar(&AccountId::try_from("acct").unwrap(), &new)
        .await;
    (made, replay)
}

#[tokio::test]
async fn a_calendar_is_made_under_the_home_and_answered_as_the_server_holds_it() {
    let (made, replay) = create(vec![wrote(201, None), ok(CREATED)]).await;

    let calendar = made.expect("created");
    assert_eq!(calendar.name, "Imported & Kept");
    assert_eq!(calendar.description.as_deref(), Some("From the source"));
    assert_eq!(calendar.color.as_deref(), Some("#FF8800FF"));
    assert!(calendar.access.may_write);

    let writes = replay.writes();
    let mkcalendar = &writes[0];
    assert_eq!(mkcalendar.method, DavMethod::Mkcalendar);
    assert_eq!(mkcalendar.precondition, Precondition::None);
    let segment = mkcalendar
        .href
        .strip_prefix("/dav/cal/alice%40test.local/")
        .and_then(|rest| rest.strip_suffix('/'))
        .expect("a collection under the home");
    assert_eq!(segment.len(), 32);
    assert!(segment.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(
        mkcalendar
            .body
            .contains("<d:displayname>Imported &amp; Kept</d:displayname>")
    );
    assert!(
        mkcalendar
            .body
            .contains("<c:calendar-description>From the source</c:calendar-description>")
    );
    assert!(
        mkcalendar
            .body
            .contains("<ic:calendar-color>#FF8800FF</ic:calendar-color>")
    );
    drop(writes);
    let reads = replay.reads();
    let read_back = reads.last().expect("read back");
    assert_eq!(read_back.0, DavMethod::Propfind);
    assert_eq!(read_back.2, "0");
}

#[test]
fn two_creates_choose_two_collections() {
    assert_ne!(super::fresh_segment(), super::fresh_segment());
}

#[tokio::test]
async fn a_bare_calendar_sends_only_its_name() {
    let body = crate::request::mkcalendar("Plain", None, None);
    assert!(body.contains("<d:displayname>Plain</d:displayname>"));
    assert!(!body.contains("calendar-description"));
    assert!(!body.contains("calendar-color"));
}

#[tokio::test]
async fn a_refused_create_says_how_and_nothing_the_server_said() {
    let (made, _) = create(vec![status(403, "<s:message>private</s:message>")]).await;
    let refused = made.unwrap_err();
    assert_eq!(refused.class(), FailureClass::Permanent);
    assert!(!refused.detail().contains("private"));
}

#[tokio::test]
async fn a_create_whose_answer_was_lost_or_not_read_back_may_exist() {
    let (made, _) = create(vec![status(502, "")]).await;
    assert!(made.unwrap_err().requires_confirmation());

    let (made, _) = create(vec![wrote(201, None), status(500, "")]).await;
    assert!(made.unwrap_err().requires_confirmation());

    let not_a_calendar = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/x/</D:href>\
        <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>\
        <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let (made, _) = create(vec![wrote(201, None), ok(not_a_calendar)]).await;
    assert!(made.unwrap_err().requires_confirmation());
}
