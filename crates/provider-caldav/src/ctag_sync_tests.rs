//! Collections whose server does not do `sync-collection`, answered the way Yahoo answers: an
//! empty multistatus without a sync-token for a calendar, a `400` for an address book, and a
//! member listing without bodies, so every body comes from a multiget.

use std::sync::Arc;

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider, IgnoreConnectSteps};

use crate::{
    CalDavError, CardDavProvider,
    carddav_tests::{BOOKS, PRINCIPAL},
    ctag_sync::refuses_sync_collection,
    sync::sync_events,
    test_support::{Replay, ok, options, status},
    transport::DavMethod,
};

const CALENDAR: &str = "/dav/cal/alice/131/";

const NO_SYNC_TOKEN: &str = r#"<D:multistatus xmlns:D="DAV:"></D:multistatus>"#;

fn ctag(collection: &str, value: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/"><D:response><D:href>{collection}</D:href><D:propstat><D:prop><CS:getctag>{value}</CS:getctag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}

/// The collection's own entry and one member, each with an ETag and nothing else.
fn listing(collection: &str, member: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>{collection}</D:href><D:propstat><D:prop><D:getetag>"c"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>{collection}{member}</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}

const EVENT: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><D:href>/dav/cal/alice/131/a.ics</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag><C:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//test//EN
BEGIN:VEVENT
UID:a@test.local
DTSTAMP:20260101T000000Z
DTSTART:20260105T090000Z
DTEND:20260105T100000Z
SUMMARY:Standup
END:VEVENT
END:VCALENDAR
</C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

const BOOK: &str = "/dav/addressbooks/alice/default/";

const CARD: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag><C:address-data>BEGIN:VCARD
VERSION:3.0
UID:ada
FN:Ada Lovelace
N:Lovelace;Ada;;;
END:VCARD
</C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

fn calendar() -> CalendarId {
    CalendarId::try_from(CALENDAR).unwrap()
}

#[tokio::test]
async fn a_calendar_whose_sync_answer_has_no_token_is_read_through_its_ctag() {
    let exec = Replay::new(vec![
        ok(NO_SYNC_TOKEN),
        ok(&ctag(CALENDAR, "c-1")),
        ok(&listing(CALENDAR, "a.ics")),
        ok(EVENT),
    ]);

    let result = sync_events(&exec, CALENDAR, &calendar(), None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &result.update else {
        panic!("a CTag read is a snapshot");
    };
    assert_eq!(objects.len(), 1, "the event arrived");
    assert_eq!(
        present.len(),
        1,
        "the collection's own entry is not a member"
    );
    assert_eq!(result.next_cursor.as_str(), "ctag:c-1");
    let reads = exec.reads();
    let steps: Vec<_> = reads
        .iter()
        .map(|(method, _, depth, _)| (*method, depth.as_str()))
        .collect();
    assert_eq!(
        steps,
        [
            (DavMethod::Report, "1"),
            (DavMethod::Propfind, "0"),
            (DavMethod::Propfind, "1"),
            (DavMethod::Report, "1"),
        ]
    );
    let multiget = &reads[3].3;
    assert!(multiget.contains("<c:calendar-multiget"), "{multiget}");
    assert!(
        !multiget.contains(&format!("<d:href>{CALENDAR}</d:href>")),
        "{multiget}"
    );
}

#[tokio::test]
async fn a_calendar_held_by_its_ctag_asks_only_for_the_ctag_while_it_stands_still() {
    let exec = Replay::new(vec![ok(&ctag(CALENDAR, "c-1"))]);
    let cursor = SyncState::new("ctag:c-1");

    let result = sync_events(&exec, CALENDAR, &calendar(), Some(&cursor))
        .await
        .unwrap();

    assert!(
        matches!(&result.update, SyncUpdate::Delta { changed, removed, .. }
            if changed.is_empty() && removed.is_empty())
    );
    assert_eq!(exec.reads().len(), 1, "no sync-collection, no listing");
}

#[tokio::test]
async fn a_calendar_whose_ctag_moved_is_read_again_in_full() {
    let exec = Replay::new(vec![
        ok(&ctag(CALENDAR, "c-2")),
        ok(&listing(CALENDAR, "a.ics")),
        ok(EVENT),
    ]);
    let cursor = SyncState::new("ctag:c-1");

    let result = sync_events(&exec, CALENDAR, &calendar(), Some(&cursor))
        .await
        .unwrap();

    // A snapshot, so whatever the listing no longer names is removed.
    assert!(result.is_snapshot());
    assert_eq!(result.next_cursor.as_str(), "ctag:c-2");
}

#[tokio::test]
async fn an_address_book_refusing_sync_collection_with_a_400_is_read_through_its_ctag() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        status(400, ""),
        ok(&ctag(BOOK, "b-1")),
        ok(&listing(BOOK, "ada.vcf")),
        ok(CARD),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();

    let result = provider
        .sync_contacts(&AccountId::try_from("account-1").unwrap(), None)
        .await
        .unwrap();

    let ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("a CTag read is a snapshot");
    };
    assert_eq!(objects.len(), 1, "the card arrived");
    assert_eq!(
        present.len(),
        1,
        "the address book's own entry is not a member"
    );
    assert_eq!(sync.next_cursor.as_str(), "ctag:b-1");
    let reads = replay.reads();
    assert!(
        reads[reads.len() - 1].3.contains("<a:addressbook-multiget"),
        "the bodies come from a multiget"
    );
    assert!(
        !reads
            .iter()
            .any(|read| read.3.contains("addressbook-query")),
        "Yahoo refuses addressbook-query, so it is never sent"
    );
}

#[test]
fn only_a_first_request_reads_a_400_as_a_refusal() {
    let bad = CalDavError::status(400, "");
    assert!(refuses_sync_collection(&bad, true));
    assert!(
        !refuses_sync_collection(&bad, false),
        "a server that issued a token does the report"
    );
    for status in [405, 501] {
        assert!(refuses_sync_collection(
            &CalDavError::status(status, ""),
            false
        ));
    }
    assert!(!refuses_sync_collection(
        &CalDavError::status(500, ""),
        true
    ));
    assert!(!refuses_sync_collection(
        &CalDavError::status(403, ""),
        true
    ));
}
