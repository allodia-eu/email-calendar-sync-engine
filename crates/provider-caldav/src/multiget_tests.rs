use std::sync::Arc;

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider, IgnoreConnectSteps};

use super::same_path;
use crate::{
    CardDavProvider,
    carddav_tests::{BOOKS, PRINCIPAL},
    sync::sync_events,
    test_support::{Replay, ok, options},
    transport::DavMethod,
};

const CALENDAR: &str = "/dav/cal/alice/default/";

/// A sync answer the way iCloud gives one: the collection's own entry, then each member with its
/// `getetag` and no `calendar-data`.
const BODILESS_SYNC: &str = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/cal/alice/default/</D:href><D:propstat><D:prop><D:getetag>"c1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/cal/alice/default/a.ics</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>token-1</D:sync-token></D:multistatus>"#;

const EVENT_BODY: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><D:href>/dav/cal/alice/default/a.ics</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag><C:calendar-data>BEGIN:VCALENDAR
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

#[tokio::test]
async fn a_member_synced_without_its_calendar_data_is_fetched_by_multiget() {
    let exec = Replay::new(vec![ok(BODILESS_SYNC), ok(EVENT_BODY)]);
    let calendar = CalendarId::try_from(CALENDAR).unwrap();

    let result = sync_events(&exec, CALENDAR, &calendar, None).await.unwrap();

    let SyncUpdate::Snapshot { objects, present } = &result.update else {
        panic!("expected a snapshot");
    };
    assert_eq!(objects.len(), 1, "the event arrived");
    assert_eq!(present.len(), 1);
    assert_eq!(result.next_cursor.as_str(), "token-1");
    let reads = exec.reads();
    let (method, href, _, body) = &reads[1];
    assert_eq!(*method, DavMethod::Report);
    assert_eq!(href, CALENDAR);
    assert!(body.contains("<c:calendar-multiget"), "{body}");
    assert!(
        body.contains("<d:href>/dav/cal/alice/default/a.ics</d:href>"),
        "{body}"
    );
    assert!(
        !body.contains("<d:href>/dav/cal/alice/default/</d:href>"),
        "the collection itself is not a member: {body}"
    );
}

const BODILESS_CARDS: &str = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:getetag>"c1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v1"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>token-1</D:sync-token></D:multistatus>"#;

const CARD_BODY: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v1"</D:getetag><C:address-data>BEGIN:VCARD
VERSION:4.0
UID:ada
FN:Ada Lovelace
END:VCARD
</C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

const NO_BODY: &str = r#"<D:multistatus xmlns:D="DAV:"/>"#;

async fn contacts_after(
    answer: &str,
) -> (SyncUpdate<engine_core::contact::ContactCard>, Arc<Replay>) {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(BODILESS_CARDS),
        ok(answer),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    let account = AccountId::try_from("account-1").unwrap();
    let ContactSourceSync::Available { sync, .. } =
        provider.sync_contacts(&account, None).await.unwrap()
    else {
        panic!("expected available");
    };
    (sync.update, replay)
}

#[tokio::test]
async fn a_card_synced_without_its_address_data_is_fetched_by_multiget() {
    let (update, replay) = contacts_after(CARD_BODY).await;

    let SyncUpdate::Snapshot { objects, .. } = update else {
        panic!("expected a snapshot");
    };
    assert_eq!(objects.len(), 1, "the card arrived");
    let reads = replay.reads();
    let (method, _, _, body) = reads.last().unwrap();
    assert_eq!(*method, DavMethod::Report);
    assert!(body.contains("<a:addressbook-multiget"), "{body}");
    assert!(body.contains("ada.vcf</d:href>"), "{body}");
}

#[tokio::test]
async fn a_card_the_multiget_returns_without_a_body_stays_present() {
    let (update, _) = contacts_after(NO_BODY).await;

    let SyncUpdate::Snapshot { objects, present } = update else {
        panic!("expected a snapshot");
    };
    assert!(objects.is_empty());
    assert!(
        present.iter().any(|key| key.as_str().ends_with("/ada.vcf")),
        "a card nobody could read was tombstoned: {present:?}"
    );
}

#[test]
fn an_absolute_href_names_the_same_resource_as_its_path() {
    assert!(same_path(
        "https://p1-dav.test:443/u/calendars/work/",
        "/u/calendars/work"
    ));
    assert!(!same_path("/u/calendars/work/a.ics", "/u/calendars/work/"));
}
