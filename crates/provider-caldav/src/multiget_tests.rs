use std::sync::Arc;

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider, IgnoreConnectSteps};

use super::{Multigets, same_path};
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

    let result = sync_events(&exec, &Multigets::default(), CALENDAR, &calendar, None)
        .await
        .unwrap();

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

/// The same event, named by the multiget with an absolute URL where the sync used a path.
const EVENT_BODY_ABSOLUTE: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><D:href>https://dav.example.net/dav/cal/alice/default/a.ics</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag><C:calendar-data>BEGIN:VCALENDAR
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
async fn a_member_keeps_the_href_the_sync_named_it_by() {
    // The store holds the event under the sync's href; a key taken from the multiget's spelling
    // would tombstone it and add it again under a new one.
    let exec = Replay::new(vec![ok(BODILESS_SYNC), ok(EVENT_BODY_ABSOLUTE)]);
    let calendar = CalendarId::try_from(CALENDAR).unwrap();

    let result = sync_events(&exec, &Multigets::default(), CALENDAR, &calendar, None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &result.update else {
        panic!("expected a snapshot");
    };
    assert_eq!(objects[0].id.as_str(), "/dav/cal/alice/default/a.ics");
    assert!(
        present
            .iter()
            .all(|key| key.as_str() == "/dav/cal/alice/default/a.ics")
    );
}

#[tokio::test]
async fn an_event_the_multiget_returns_without_a_body_stays_present() {
    let exec = Replay::new(vec![ok(BODILESS_SYNC), ok(NO_BODY)]);
    let calendar = CalendarId::try_from(CALENDAR).unwrap();

    let result = sync_events(&exec, &Multigets::default(), CALENDAR, &calendar, None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &result.update else {
        panic!("expected a snapshot");
    };
    assert!(objects.is_empty());
    assert_eq!(
        present.len(),
        1,
        "an event nobody could read was tombstoned, or the collection counted: {present:?}"
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
    assert_eq!(
        present
            .iter()
            .map(engine_core::ids::ProviderKey::as_str)
            .collect::<Vec<_>>(),
        ["/dav/addressbooks/alice/default/ada.vcf"],
        "a card nobody could read was tombstoned, or the address book counted as a card"
    );
}

/// The multiget's answer for a member removed after the sync listed it.
const GONE: &str = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/cal/alice/default/a.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response></D:multistatus>"#;

#[tokio::test]
async fn a_member_the_multiget_answers_404_for_is_removed() {
    let exec = Replay::new(vec![ok(BODILESS_SYNC), ok(GONE)]);
    let calendar = CalendarId::try_from(CALENDAR).unwrap();

    let result = sync_events(&exec, &Multigets::default(), CALENDAR, &calendar, None)
        .await
        .unwrap();

    let SyncUpdate::Snapshot { objects, present } = &result.update else {
        panic!("expected a snapshot");
    };
    assert!(objects.is_empty());
    assert!(
        present.is_empty(),
        "the server said it is gone: {present:?}"
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

/// Counts how many requests are in flight at once.
#[derive(Default)]
struct Overlapping {
    in_flight: std::sync::atomic::AtomicUsize,
    most: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::transport::DavExecutor for Overlapping {
    async fn send(
        &self,
        _method: DavMethod,
        _href: &str,
        _depth: &str,
        _body: String,
    ) -> Result<crate::transport::HttpResponse, crate::error::CalDavError> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.most.fetch_max(now, SeqCst);
        // Give the other requests the chance to start while this one is still out.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        self.in_flight.fetch_sub(1, SeqCst);
        Ok(ok(r#"<D:multistatus xmlns:D="DAV:"/>"#))
    }

    async fn send_write(
        &self,
        _request: crate::transport::WriteRequest,
    ) -> Result<crate::transport::HttpResponse, crate::error::CalDavError> {
        unreachable!("a multiget writes nothing")
    }
}

/// A sync answer of `count` bodiless members of `collection`.
fn bodiless(collection: &str, count: usize) -> crate::dav::MultiStatus {
    let mut members = String::new();
    for n in 0..count {
        use std::fmt::Write as _;
        write!(
            members,
            "<D:response><D:href>{collection}{n}.ics</D:href><D:propstat><D:prop>\
             <D:getetag>\"e{n}\"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status>\
             </D:propstat></D:response>"
        )
        .unwrap();
    }
    crate::dav::parse_multistatus(&format!(
        r#"<D:multistatus xmlns:D="DAV:">{members}</D:multistatus>"#
    ))
    .unwrap()
}

#[tokio::test]
async fn the_multigets_of_a_large_collection_are_in_flight_together() {
    // Five hundred bodiless members: five multigets of a hundred.
    let mut report = bodiless(CALENDAR, 500);
    let exec = Overlapping::default();

    super::fill_bodies(
        &exec,
        &Multigets::default(),
        CALENDAR,
        &mut report,
        "calendar-data",
        crate::request::calendar_multiget_report,
    )
    .await
    .unwrap();

    assert_eq!(
        exec.most.load(std::sync::atomic::Ordering::SeqCst),
        super::IN_FLIGHT,
        "as many multigets out at once as the bound allows, and no more"
    );
}

#[tokio::test]
async fn collections_synced_at_once_share_one_bound() {
    // The engine syncs several calendars at once; each sending its own four would put eight
    // on the server here.
    const WORK: &str = "/dav/cal/alice/work/";
    let (mut home, mut work) = (bodiless(CALENDAR, 500), bodiless(WORK, 500));
    let exec = Overlapping::default();
    let multigets = Multigets::default();

    let (a, b) = tokio::join!(
        super::fill_bodies(
            &exec,
            &multigets,
            CALENDAR,
            &mut home,
            "calendar-data",
            crate::request::calendar_multiget_report,
        ),
        super::fill_bodies(
            &exec,
            &multigets,
            WORK,
            &mut work,
            "calendar-data",
            crate::request::calendar_multiget_report,
        ),
    );
    a.unwrap();
    b.unwrap();

    assert_eq!(
        exec.most.load(std::sync::atomic::Ordering::SeqCst),
        super::IN_FLIGHT
    );
}
