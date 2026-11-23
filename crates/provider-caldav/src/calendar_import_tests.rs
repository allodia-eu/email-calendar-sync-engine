//! The CalDAV importer over replayed answers each harness server gave to the exact requests it
//! sends (`tests/fixtures/import/`). The replay answers whatever it is sent, so the request
//! shapes are asserted here and proven live in `tests/common/import.rs`.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, CalendarId, EventId},
    raw::RawIcal,
    sync::SyncState,
};
use engine_provider::{
    IgnoreConnectSteps, ProviderResult,
    import::{CalendarImporter, EventImport, ImportedEvent},
};

use crate::{
    CalDavProvider,
    href::resource_name,
    response::HttpResponse,
    test_support::{Replay, ok, options, status, wrote},
    transport::{DavMethod, Precondition},
};

pub(super) const PRINCIPAL: &str = include_str!("../tests/fixtures/principal.xml");
pub(super) const PROPS: &str = include_str!("../tests/fixtures/import/stalwart-import-props.xml");
const SABRE_PROPS: &str = include_str!("../tests/fixtures/import/sabredav-import-props.xml");
pub(super) const NONE_HELD: &str =
    include_str!("../tests/fixtures/import/stalwart-uid-query-empty.xml");
pub(super) const HELD: &str = include_str!("../tests/fixtures/import/stalwart-uid-query.xml");
const HELD_TWICE: &str = include_str!("../tests/fixtures/import/sabredav-uid-query.xml");
const STALWART_CONFLICT: &str =
    include_str!("../tests/fixtures/import/stalwart-put-second-uid.xml");
const NEXTCLOUD_CONFLICT: &str =
    include_str!("../tests/fixtures/import/nextcloud-put-second-uid.xml");
const SABRE_TAKEN: &str = include_str!("../tests/fixtures/import/sabredav-put-again.xml");
pub(super) const STALWART_INVALID: &str =
    include_str!("../tests/fixtures/import/stalwart-put-invalid.xml");
pub(super) const NEXTCLOUD_INVALID: &str =
    include_str!("../tests/fixtures/import/nextcloud-put-invalid.xml");

/// The object the fixtures hold, as the source held it.
pub(super) const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Capture//EN\r\nBEGIN:VEVENT\r\n\
UID:capture-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20270105T100000Z\r\n\
DTEND:20270105T110000Z\r\nSUMMARY:Captured\r\nORGANIZER:mailto:bob@example.com\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:carol@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// Where the Stalwart fixtures say the held object is.
pub(super) const HELD_HREF: &str =
    "/dav/cal/bob%40test.local/laneb-capture/capture-1@example.com.ics";

pub(super) fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

pub(super) fn work() -> CalendarId {
    CalendarId::try_from("/dav/cal/alice%40test.local/work/").unwrap()
}

pub(super) fn ours() -> EventId {
    EventId::try_from("/dav/cal/alice%40test.local/work/capture-1@example.com.ics").unwrap()
}

pub(super) fn event(text: &str) -> EventImport {
    EventImport::new(RawIcal::new(text), work())
}

pub(super) fn key(answer: &ImportedEvent) -> Option<&str> {
    answer.key.as_ref().map(EventId::as_str)
}

/// A provider whose discovery is replayed, answering `after` in order once connected, with the
/// replay kept to read what it was sent.
pub(super) async fn target(after: Vec<HttpResponse>) -> (CalDavProvider, Arc<Replay>) {
    target_advertising("1, 3, calendar-access, calendar-auto-schedule", after).await
}

/// [`target`] on a server whose calendar home advertises `classes` (RFC 4918 §10.1).
pub(super) async fn target_advertising(
    classes: &str,
    after: Vec<HttpResponse>,
) -> (CalDavProvider, Arc<Replay>) {
    let mut responses = vec![ok(PRINCIPAL), options(Some(classes))];
    responses.extend(after);
    let replay = Arc::new(Replay::new(responses));
    let provider = CalDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    (provider, replay)
}

pub(super) async fn import_one(
    after: Vec<HttpResponse>,
) -> (ProviderResult<ImportedEvent>, Arc<Replay>) {
    let (provider, replay) = target(after).await;
    let mut answers = provider.import_events(&account(), &[event(EVENT)]).await;
    assert_eq!(answers.len(), 1);
    (answers.remove(0), replay)
}

pub(super) fn lookups(replay: &Replay) -> usize {
    replay
        .seen()
        .iter()
        .filter(|(method, _)| *method == DavMethod::Report)
        .count()
}

#[tokio::test]
async fn an_event_nobody_holds_is_stored_where_its_uid_names_it_telling_nobody() {
    let (answer, replay) =
        import_one(vec![ok(PROPS), ok(NONE_HELD), wrote(201, Some("\"e1\""))]).await;

    let stored = answer.expect("stored");
    assert_eq!(stored.key, Some(ours()));
    assert!(!stored.already_there);
    assert_eq!(
        stored
            .revisions
            .etag
            .as_ref()
            .map(|etag| etag.as_str().to_owned()),
        Some("\"e1\"".to_owned())
    );
    let reads = replay.reads();
    let query = reads
        .iter()
        .find(|read| read.0 == DavMethod::Report)
        .expect("a lookup");
    assert_eq!(query.1, "/dav/cal/alice%40test.local/work/");
    assert_eq!(query.2, "1");
    assert!(
        query.3.contains(">capture-1@example.com</c:text-match>"),
        "{}",
        query.3
    );
    let writes = replay.writes();
    assert_eq!(writes.len(), 1);
    let put = &writes[0];
    assert_eq!(put.method, DavMethod::Put);
    assert_eq!(put.href, ours().as_str());
    assert_eq!(put.precondition, Precondition::IfNoneMatch);
    assert_eq!(put.content_type, Some("text/calendar; charset=utf-8"));
    assert!(
        put.body
            .contains("ORGANIZER;SCHEDULE-AGENT=CLIENT:mailto:bob@example.com")
    );
    assert!(
        put.body
            .contains("PARTSTAT=NEEDS-ACTION;SCHEDULE-AGENT=CLIENT:mailto:carol")
    );
}

#[tokio::test]
async fn a_uid_the_calendar_holds_is_already_there_and_nothing_is_written() {
    let (answer, replay) = import_one(vec![ok(PROPS), ok(HELD)]).await;

    let held = answer.expect("linked");
    assert!(held.already_there);
    assert_eq!(key(&held), Some(HELD_HREF));
    assert!(
        replay.writes().is_empty(),
        "a held UID is never stored twice"
    );
}

#[tokio::test]
async fn a_uid_that_only_contains_the_one_asked_for_is_not_a_match() {
    // `text-match` is a substring match, so asking for `capture-1` finds `capture-1@…` too.
    let short = EVENT.replace("UID:capture-1@example.com", "UID:capture-1");
    let (provider, replay) = target(vec![ok(PROPS), ok(HELD_TWICE), wrote(201, None)]).await;

    let answers = provider.import_events(&account(), &[event(&short)]).await;

    assert!(!answers[0].as_ref().expect("stored").already_there);
    assert_eq!(replay.writes().len(), 1);
}

#[tokio::test]
async fn a_name_already_taken_by_this_uid_is_an_earlier_attempt_of_this_import() {
    for taken in [status(412, ""), status(412, SABRE_TAKEN)] {
        let (answer, replay) =
            import_one(vec![ok(PROPS), ok(NONE_HELD), taken, status(200, EVENT)]).await;
        assert_eq!(
            answer.expect("linked"),
            ImportedEvent::existing(Some(ours()))
        );
        let reads = replay.reads();
        let read = reads.last().expect("the name was read");
        assert_eq!((read.0, read.1.as_str()), (DavMethod::Get, ours().as_str()));
    }
}

#[tokio::test]
async fn a_second_uid_refused_by_the_server_links_to_the_object_it_names() {
    let (answer, _) = import_one(vec![
        ok(PROPS),
        ok(NONE_HELD),
        status(403, STALWART_CONFLICT),
    ])
    .await;
    let linked = answer.expect("linked");
    assert!(linked.already_there);
    assert_eq!(key(&linked), Some(HELD_HREF));

    let (answer, replay) = import_one(vec![
        ok(PROPS),
        ok(NONE_HELD),
        status(409, NEXTCLOUD_CONFLICT),
    ])
    .await;
    let linked = answer.expect("linked");
    assert!(linked.already_there);
    assert_eq!(
        key(&linked),
        Some("/remote.php/dav/calendars/alice/laneb-capture/capture-1@example.com.ics")
    );
    assert_eq!(lookups(&replay), 1, "the refusal named the holder");
}

#[tokio::test]
async fn a_refusal_naming_no_holder_asks_for_it() {
    let bare = "<d:error xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
                <c:no-uid-conflict/></d:error>";
    let (answer, replay) =
        import_one(vec![ok(PROPS), ok(NONE_HELD), status(409, bare), ok(HELD)]).await;

    assert_eq!(key(&answer.expect("linked")), Some(HELD_HREF));
    assert_eq!(lookups(&replay), 2);
}

#[tokio::test]
async fn an_object_the_server_refuses_will_never_be_taken_and_says_nothing_it_said() {
    for refusal in [
        status(412, STALWART_INVALID),
        status(403, NEXTCLOUD_INVALID),
        status(415, "<s:message>private summary</s:message>"),
        status(413, ""),
        status(400, ""),
    ] {
        let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), refusal]).await;
        let refused = answer.expect_err("refused");
        assert!(refused.is_item_unavailable(), "{refused:?}");
        assert!(
            !refused.detail().contains("private"),
            "{}",
            refused.detail()
        );
        assert!(!refused.detail().contains("Sabre"), "{}", refused.detail());
        assert!(std::error::Error::source(&refused).is_none());
    }
}

#[tokio::test]
async fn a_server_error_is_retryable_and_a_silent_gateway_may_have_stored_it() {
    let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), status(503, "busy")]).await;
    let failed = answer.unwrap_err();
    assert_eq!(failed.class(), FailureClass::Retryable);
    assert!(!failed.detail().contains("busy"));

    let (answer, _) = import_one(vec![ok(PROPS), ok(NONE_HELD), status(504, "")]).await;
    assert!(answer.unwrap_err().requires_confirmation());
}

#[tokio::test]
async fn an_event_over_the_calendars_limit_is_refused_before_anything_is_sent() {
    let big = EVENT.replace(
        "SUMMARY:Captured",
        &format!("SUMMARY:{}", "x".repeat(600_000)),
    );
    let (provider, replay) = target(vec![ok(PROPS)]).await;

    let answers = provider.import_events(&account(), &[event(&big)]).await;

    assert!(answers[0].as_ref().unwrap_err().is_item_unavailable());
    assert!(replay.writes().is_empty());
}

#[tokio::test]
async fn an_object_that_is_not_one_event_is_refused_before_anything_is_sent() {
    let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t@example.com\r\n\
                END:VTODO\r\nEND:VCALENDAR\r\n";
    let (provider, replay) = target(vec![ok(PROPS)]).await;

    let answers = provider.import_events(&account(), &[event(todo)]).await;

    assert!(answers[0].as_ref().unwrap_err().is_item_unavailable());
    assert_eq!(
        replay.seen().len(),
        3,
        "discovery, OPTIONS and the limits alone"
    );
}

#[tokio::test]
async fn a_calendar_that_is_not_an_href_is_refused_before_anything_is_sent() {
    let (provider, replay) = target(vec![ok(PROPS)]).await;
    let nameless = EventImport::new(RawIcal::new(EVENT), CalendarId::try_from("work").unwrap());

    let answers = provider.import_events(&account(), &[nameless]).await;

    assert_eq!(
        answers[0].as_ref().unwrap_err().class(),
        FailureClass::Permanent
    );
    assert!(replay.writes().is_empty());
}

#[tokio::test]
async fn a_lookup_the_server_will_not_answer_leaves_the_put_to_its_own_refusal() {
    let (answer, replay) = import_one(vec![ok(PROPS), status(400, ""), wrote(201, None)]).await;
    assert!(!answer.expect("stored").already_there);
    assert_eq!(replay.writes().len(), 1);
}

#[tokio::test]
async fn a_lookup_that_fails_answers_the_event_unstored() {
    for failure in [status(503, ""), status(401, ""), status(207, "<not xml")] {
        let (answer, replay) = import_one(vec![ok(PROPS), failure]).await;
        assert!(answer.is_err());
        assert!(replay.writes().is_empty());
    }
}

#[tokio::test]
async fn limits_that_cannot_be_read_are_none_and_each_calendar_is_asked_once() {
    let other = CalendarId::try_from("/dav/cal/alice%40test.local/home/").unwrap();
    let (provider, replay) = target(vec![
        status(500, ""),
        ok(NONE_HELD),
        wrote(201, None),
        ok(NONE_HELD),
        wrote(201, None),
        ok(SABRE_PROPS),
        ok(NONE_HELD),
        wrote(201, None),
    ])
    .await;
    let second = EVENT.replace("capture-1@", "capture-2@");
    let third = EVENT.replace("capture-1@", "capture-3@");
    let batch = [
        event(EVENT),
        event(&second),
        EventImport::new(RawIcal::new(third), other),
    ];

    let answers = provider.import_events(&account(), &batch).await;

    assert_eq!(answers.len(), 3);
    assert!(answers.iter().all(Result::is_ok), "{answers:?}");
    let propfinds = replay
        .seen()
        .iter()
        .filter(|(method, _)| *method == DavMethod::Propfind)
        .count();
    assert_eq!(propfinds, 3, "discovery, then one per calendar");
}

#[tokio::test]
async fn the_cursor_is_the_calendars_sync_token_or_its_ctag() {
    let (provider, replay) = target(vec![
        ok(PROPS),
        ok(
            "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"http://calendarserver.org/ns/\"><D:response>\
             <D:href>/x/</D:href><D:propstat><D:prop><D:sync-token/><C:getctag>7</C:getctag>\
             </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
        ),
        ok("<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/x/</D:href></D:response>\
            </D:multistatus>"),
        status(404, ""),
    ])
    .await;

    assert_eq!(
        provider.import_cursor(&account(), &work()).await.unwrap(),
        SyncState::new("urn:stalwart:davsync:463")
    );
    assert_eq!(
        provider.import_cursor(&account(), &work()).await.unwrap(),
        SyncState::new("ctag:7")
    );
    let asked = replay.reads().last().expect("asked").3.clone();
    assert!(asked.contains("<d:sync-token/>") && asked.contains("<cs:getctag/>"));
    for _ in 0..2 {
        assert_eq!(
            provider
                .import_cursor(&account(), &work())
                .await
                .unwrap_err()
                .class(),
            FailureClass::Permanent
        );
    }
}

#[tokio::test]
async fn the_limits_are_the_calendars_largest_object() {
    let (provider, _) = target(vec![ok(PROPS), ok(SABRE_PROPS), status(403, "")]).await;

    let stalwart = provider
        .calendar_import_limits(&account(), &work())
        .await
        .unwrap();
    let sabre = provider
        .calendar_import_limits(&account(), &work())
        .await
        .unwrap();

    assert_eq!(stalwart.max_event_size().map(u64::from), Some(524_288));
    assert_eq!(sabre.max_event_size().map(u64::from), Some(10_000_000));
    assert!(
        provider
            .calendar_import_limits(&account(), &work())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_importer_is_offered_where_the_server_promises_to_honour_the_suppression() {
    let (scheduling, _) = target(Vec::new()).await;
    let held: Box<dyn engine_provider::Provider> = Box::new(scheduling);
    assert!(held.calendar_importer().is_some());
    assert!(held.calendar_collections().is_some());

    // RFC 6638 §2: a server that schedules advertises it, and only such a server is bound by
    // §7.1 to leave a `SCHEDULE-AGENT=CLIENT` party to the client.
    let (plain, _) = target_advertising("1, 3, calendar-access", Vec::new()).await;
    let held: Box<dyn engine_provider::Provider> = Box::new(plain);
    assert!(held.calendar_importer().is_none());
    assert!(held.calendar_collections().is_some());
}

#[test]
fn a_uid_names_its_resource_verbatim_until_nextcloud_could_not_hold_it() {
    assert_eq!(
        resource_name("capture-1@example.com"),
        "capture-1@example.com"
    );
    // An Outlook meeting UID, 112 hex characters, is its own name.
    let outlook = format!("040000008200E00074C5B7101A82E008{}", "0".repeat(80));
    assert_eq!(resource_name(&outlook), outlook);
    // 251 bytes plus `.ics` is the longest name Nextcloud stored (measured).
    let longest = "u".repeat(251);
    assert_eq!(resource_name(&longest), longest);
    let hashed = resource_name(&"u".repeat(252));
    assert!(hashed.starts_with("sha256-"), "{hashed}");
    assert_eq!(hashed.len(), "sha256-".len() + 64);
    assert_eq!(
        hashed,
        resource_name(&"u".repeat(252)),
        "the same UID, the same name"
    );
}
