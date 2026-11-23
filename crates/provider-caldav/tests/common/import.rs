//! The live calendar import scenario, run against every harness server (Stalwart, SabreDAV,
//! Nextcloud): create a calendar, import into it, import again, meet a `UID` someone else
//! stored, and find it all through the target's own delta from the import cursor.
//!
//! What only a real server can answer, and why each step is here:
//!
//! - `MKCALENDAR` with a name, a description and a colour, and whether the server keeps them (the
//!   read-back is the receipt).
//! - Whether the importer's `PUT`, `calendar-query` and `PROPFIND` are shapes the server takes.
//! - **The `UID` lookup.** SabreDAV stores a second object with a `UID` it holds
//!   (`docs/spec-deviations.md`, SABREDAV-1), so on that server the lookup is the only thing
//!   between an import and a duplicate; [`a_uid_someone_else_stored_is_linked`] is the check that
//!   fails there without it.
//! - That the stored meeting carries `SCHEDULE-AGENT=CLIENT` on every party the server reads. That
//!   nobody is *told* is a separate live proof, with a control that is told
//!   (`tests/scheduling/import.rs`, Stalwart).

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId, EventId, Uid},
    raw::RawIcal,
    sync::SyncUpdate,
};
use engine_provider::{
    CalendarSync, CalendarWrites, EventWrite, NewCalendar, Provider,
    import::{EventImport, ImportedEvent},
};
use provider_caldav::CalDavProvider;

/// Where a harness server is and who signs in, for the one request the engine has no verb for:
/// removing the scratch calendar again.
pub(crate) struct DavServer {
    pub(crate) base: String,
    pub(crate) user: String,
    pub(crate) pass: String,
}

/// The name every scratch calendar this scenario makes is given, so a run interrupted before its
/// cleanup is swept by the next.
const SCRATCH: &str = "Import scratch (live suite)";

/// A recurring meeting with a moved occurrence: the override has parties of its own.
fn meeting(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Live suite//Import//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\n\
         DTSTART;TZID=Europe/Amsterdam:20270104T100000\r\n\
         DTEND;TZID=Europe/Amsterdam:20270104T103000\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
         SUMMARY:Imported standup\r\n\
         ORGANIZER;CN=Organizer:mailto:organizer@example.com\r\n\
         ATTENDEE;CN=Guest;PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT:mailto:guest@example.com\r\n\
         BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Soon\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\n\
         END:VEVENT\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\n\
         RECURRENCE-ID;TZID=Europe/Amsterdam:20270105T100000\r\n\
         DTSTART;TZID=Europe/Amsterdam:20270105T110000\r\n\
         DTEND;TZID=Europe/Amsterdam:20270105T113000\r\nSUMMARY:Imported standup (moved)\r\n\
         ORGANIZER;CN=Organizer:mailto:organizer@example.com\r\n\
         ATTENDEE;CN=Guest;PARTSTAT=TENTATIVE:mailto:guest@example.com\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// An all-day event with no parties.
fn holiday(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Live suite//Import//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\n\
         DTSTART;VALUE=DATE:20270301\r\nDTEND;VALUE=DATE:20270302\r\n\
         SUMMARY:Imported holiday\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// A task, which is not an event.
const TASK: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Live suite//Import//EN\r\n\
                    BEGIN:VTODO\r\nUID:live-import-task@example.com\r\n\
                    DTSTAMP:20260101T000000Z\r\nSUMMARY:Not an event\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";

/// The whole scenario: create, import, re-import, link, and read it back from the cursor.
pub(crate) async fn import_round_trip(
    provider: &CalDavProvider,
    account: &AccountId,
    server: &DavServer,
) {
    sweep(provider, account, server).await;
    let new = NewCalendar::new(SCRATCH)
        .with_description("Imported by the live suite")
        .with_color("#FF8800FF");
    let creator = provider
        .calendar_collections()
        .expect("CalDAV creates calendars");
    let calendar = creator
        .create_calendar(account, &new)
        .await
        .expect("MKCALENDAR");
    assert_eq!(calendar.name, SCRATCH);
    assert_eq!(
        calendar.description.as_deref(),
        Some("Imported by the live suite")
    );
    assert_eq!(calendar.color.as_deref(), Some("#FF8800FF"));
    assert!(calendar.access.may_write);

    let outcome = imports_into(provider, account, &calendar.id).await;
    remove(server, &calendar.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Runs the import steps, catching a failure so the calendar is removed either way.
async fn imports_into(
    provider: &CalDavProvider,
    account: &AccountId,
    calendar: &CalendarId,
) -> Result<(), Box<dyn std::any::Any + Send>> {
    use futures_util::FutureExt;
    std::panic::AssertUnwindSafe(steps(provider, account, calendar))
        .catch_unwind()
        .await
}

async fn steps(provider: &CalDavProvider, account: &AccountId, calendar: &CalendarId) {
    // Every harness server is one where silence is known: Stalwart and Nextcloud advertise
    // scheduling (RFC 6638 §7.1), and the SabreDAV fixture's principal has no scheduling inbox or
    // outbox, so it has nothing to send with.
    let importer = provider
        .calendar_importer()
        .expect("CalDAV imports where the import is known to tell nobody");
    let limits = importer
        .calendar_import_limits(account, calendar)
        .await
        .expect("limits");
    assert!(
        limits.max_event_size().is_some(),
        "every harness states one"
    );
    let cursor = importer
        .import_cursor(account, calendar)
        .await
        .expect("cursor");

    let (standup, day) = (
        "live-import-standup@example.com",
        "live-import-holiday@example.com",
    );
    let batch = [
        EventImport::new(RawIcal::new(meeting(standup)), calendar.clone()),
        EventImport::new(RawIcal::new(holiday(day)), calendar.clone()),
        EventImport::new(RawIcal::new(TASK), calendar.clone()),
        EventImport::new(RawIcal::new(meeting(standup)), calendar.clone()),
    ];
    let answers = importer.import_events(account, &batch).await;
    assert_eq!(answers.len(), 4);
    let first = answers[0].as_ref().expect("the meeting is stored");
    assert!(!first.already_there);
    assert!(first.key.is_some(), "CalDAV names what it stored");
    assert!(
        !answers[1]
            .as_ref()
            .expect("the day is stored")
            .already_there
    );
    assert!(
        answers[2].as_ref().unwrap_err().is_item_unavailable(),
        "a task is not an event"
    );
    assert_eq!(
        answers[3].as_ref().expect("the second copy is linked"),
        &ImportedEvent::existing(first.key.clone()),
        "a UID the batch just stored is not stored twice"
    );

    let again = importer.import_events(account, &batch[..2]).await;
    for (answer, earlier) in again.iter().zip(&answers) {
        let answer = answer.as_ref().expect("a re-import is answered");
        assert!(answer.already_there, "a re-import stores nothing");
        assert_eq!(answer.key, earlier.as_ref().unwrap().key);
    }

    let foreign = a_uid_someone_else_stored_is_linked(provider, account, calendar).await;

    let delta = provider
        .sync_events(account, calendar, Some(&cursor), super::whole())
        .await
        .expect("the delta from the import cursor");
    let listed: Vec<Event> = match delta.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    let uids: Vec<&str> = listed.iter().map(|event| event.uid.as_str()).collect();
    for uid in [standup, day, foreign.as_str()] {
        assert_eq!(
            uids.iter().filter(|listed| **listed == uid).count(),
            1,
            "{uid} once in the delta: {uids:?}"
        );
    }
    let stored = listed
        .iter()
        .find(|event| event.uid.as_str() == standup)
        .expect("the meeting");
    // The id the import answered is the id the target lists: what a host records is what it
    // will read back.
    for (answer, uid) in answers.iter().zip([standup, day]) {
        let listed_id = &listed
            .iter()
            .find(|event| event.uid.as_str() == uid)
            .expect("listed")
            .id;
        assert_eq!(
            answer.as_ref().unwrap().key.as_ref(),
            Some(listed_id),
            "{uid}"
        );
    }
    stored_as_sent(stored, &meeting(standup), calendar);
}

/// An object stored under another name with a `UID` the import then brings: linked to it,
/// never stored beside it. Returns that `UID`.
async fn a_uid_someone_else_stored_is_linked(
    provider: &CalDavProvider,
    account: &AccountId,
    calendar: &CalendarId,
) -> String {
    let uid = "live-import-foreign@example.com";
    let elsewhere =
        EventId::try_from(format!("{}someone-else.ics", calendar.as_str()).as_str()).unwrap();
    provider
        .put_event(
            account,
            &EventWrite::creating(
                elsewhere.clone(),
                Uid::new(uid).unwrap(),
                RawIcal::new(holiday(uid)),
            ),
        )
        .await
        .expect("another client stores the event");
    let answers = provider
        .calendar_importer()
        .expect("CalDAV imports")
        .import_events(
            account,
            &[EventImport::new(
                RawIcal::new(holiday(uid)),
                calendar.clone(),
            )],
        )
        .await;
    let linked = answers[0].as_ref().expect("answered");
    assert!(linked.already_there, "a held UID is linked: {linked:?}");
    assert_eq!(linked.key.as_ref(), Some(&elsewhere));
    uid.to_owned()
}

/// The meeting as stored reads as the one sent, every party left to the client.
fn stored_as_sent(stored: &Event, sent: &str, calendar: &CalendarId) {
    let ical = stored
        .raw_ical
        .as_ref()
        .expect("raw")
        .as_str()
        .replace("\r\n ", "")
        .replace("\n ", "");
    let parties: Vec<&str> = ical
        .lines()
        .filter(|line| line.starts_with("ORGANIZER") || line.starts_with("ATTENDEE"))
        .collect();
    assert_eq!(
        parties.len(),
        4,
        "master and override, organizer and guest: {ical}"
    );
    assert!(
        parties
            .iter()
            .all(|line| line.to_ascii_uppercase().contains("SCHEDULE-AGENT=CLIENT")),
        "{parties:?}"
    );
    let original = engine_ical::parse_calendar_object(sent, stored.id.clone(), calendar.clone())
        .expect("the source parses");
    assert_eq!(stored.title, original.title);
    assert_eq!(stored.start, original.start);
    assert_eq!(stored.participants.len(), original.participants.len());
    assert_eq!(stored.recurrence, original.recurrence);
    assert_eq!(stored.alerts.len(), original.alerts.len());
}

/// Removes every scratch calendar an interrupted run left behind.
async fn sweep(provider: &CalDavProvider, account: &AccountId, server: &DavServer) {
    for calendar in super::calendars(provider, account).await {
        if calendar.name == SCRATCH {
            remove(server, &calendar.id).await;
        }
    }
}

/// `DELETE` of a calendar collection, which no engine verb does.
async fn remove(server: &DavServer, calendar: &CalendarId) {
    let url = format!("{}{}", server.base, calendar.as_str());
    let answer = reqwest::Client::new()
        .delete(url)
        .basic_auth(&server.user, Some(&server.pass))
        .send()
        .await
        .expect("DELETE the scratch calendar");
    assert!(answer.status().is_success(), "{}", answer.status());
}
