// SPDX-License-Identifier: MPL-2.0
// Sync marshaler tests (class routing, change outcomes, estimates).

use super::*;
use crate::{
    calendar::{
        CAL_ALL_DAY_EVENT, CAL_BUSY_STATUS, CAL_END_TIME, CAL_START_TIME, CAL_SUBJECT,
        CalendarAttendee, CalendarEventProps, CalendarException, CalendarRecurrence, PAGE_CALENDAR,
        TimeZoneBlob, TziTimeZone,
        tests::{TZI_FLAT_UTC8, fixture_full_app_data},
    },
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CHANGE, AS_COLLECTION, AS_COLLECTIONS, AS_COMMANDS,
        AS_DELETE, AS_MORE_AVAILABLE, AS_SERVER_ID, AS_STATUS, AS_SYNC, AS_SYNC_KEY, PAGE_AIRSYNC,
    },
    contacts::{CON_FILE_AS, ContactsContactProps, PAGE_CONTACTS},
    contacts_testutil::{expected_full_contact_props, fixture_full_contact_app_data},
    wbxml::WbxmlElement,
};

/// All-day Calendar ApplicationData (shape of the Task-2
/// `parse_all_day_item` fixture). Deliberately DIFFERENT from the full
/// fixture so a crossed wire between the Add and Change items cannot
/// hide behind equal props.
fn all_day_app_data() -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![
            WbxmlElement::text(PAGE_CALENDAR, CAL_SUBJECT, "Company Holiday"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_ALL_DAY_EVENT, "1"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, "20260820T000000Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_END_TIME, "20260821T000000Z"),
            WbxmlElement::text(PAGE_CALENDAR, CAL_BUSY_STATUS, "0"),
        ],
    )
}

/// Golden props for [`fixture_full_app_data`] — mirrors the
/// `parse_full_core_item` assertion in calendar.rs so the seam test
/// locks the FULL props fidelity end-to-end through the Sync envelope.
fn expected_full_props() -> CalendarEventProps {
    CalendarEventProps {
        all_day_event: false,
        start_time: Some("20260818T090000Z".to_string()),
        end_time: Some("20260818T100000Z".to_string()),
        dtstamp: Some("20260815T120000Z".to_string()),
        subject: Some("Weekly Sync".to_string()),
        location: Some("Room 42".to_string()),
        body_plain: Some("Agenda: sync status".to_string()),
        organizer_name: Some("Felix Zhou".to_string()),
        organizer_email: Some("felixzhou@kylins.local".to_string()),
        sensitivity: Some(2),
        busy_status: Some(2),
        reminder_set: true,
        reminder_minutes: Some(15),
        meeting_status: Some(1),
        response_requested: true,
        uid: None,
        time_zone: Some(TimeZoneBlob {
            raw_base64: Some(TZI_FLAT_UTC8.to_string()),
            parsed: Some(TziTimeZone {
                base_bias_minutes: -480,
                standard: None,
                daylight: None,
            }),
        }),
        recurrence: Some(CalendarRecurrence {
            recurrence_type: 1,
            interval: Some(1),
            day_of_week: Some(62),
            until: Some("20261225T090000Z".to_string()),
            no_end: false,
            ..Default::default()
        }),
        exceptions: vec![
            CalendarException {
                deleted: true,
                exception_start_time: Some("20260825T090000Z".to_string()),
                ..Default::default()
            },
            CalendarException {
                deleted: false,
                exception_start_time: Some("20260901T090000Z".to_string()),
                start_time: Some("20260901T100000Z".to_string()),
                end_time: Some("20260901T110000Z".to_string()),
                subject: Some("Moved".to_string()),
                location: Some("Room 7".to_string()),
                body_plain: None,
                // The fixture carries AllDayEvent "0" → Some(false)
                // (interlude-A tri-state; absence parses to None).
                all_day_event: Some(false),
            },
        ],
        attendees: vec![
            CalendarAttendee {
                name: Some("Bob".to_string()),
                email: "bob@example.com".to_string(),
                status: Some(3),
            },
            CalendarAttendee {
                name: Some("Carol".to_string()),
                email: "carol@example.com".to_string(),
                status: None,
            },
        ],
    }
}

/// Calendar-class Sync response fixture: Collection with SyncKey
/// "{cal-sk1}", Status "1", MoreAvailable, and a Commands block with
/// one Add (ServerId "cal:1" + the Task-2/3 FULL ApplicationData
/// fixture), one Change (ServerId "cal:2" + the all-day fixture), one
/// Delete (ServerId "cal:3" — deletes are class-agnostic on the wire).
fn fixture_calendar_sync_response() -> WbxmlElement {
    let add = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:1"),
            fixture_full_app_data(),
        ],
    );
    let change = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:2"),
            all_day_app_data(),
        ],
    );
    // MS-ASCMD 2.2.3.42.2: Delete is a CONTAINER with the ServerId as
    // a child — same envelope shape as Add/Change.
    let delete = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "cal:3")],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add, change, delete]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{cal-sk1}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
            commands,
        ],
    );
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Brief test (1): a Calendar-class response routes Add/Change through
/// the MS-ASCAL parser — `calendar_added` / `calendar_updated` carry
/// the ServerIds + FULL props, deletes land in the shared
/// `deleted_server_ids`, and the Email-shaped `added` / `updated`
/// vectors stay EMPTY.
#[test]
fn calendar_class_sync_routes_items_to_calendar_vectors() {
    let tree = fixture_calendar_sync_response();

    let result = parse_sync_response_for_class(&tree, "Calendar").expect("parse");

    // Envelope fields parse identically for the Calendar class.
    assert_eq!(result.sync_key, "{cal-sk1}");
    assert_eq!(result.status, 1);
    assert!(result.more_available);

    // Email-shaped vectors stay empty — no double delivery.
    assert!(
        result.added.is_empty(),
        "Calendar class must not fill added"
    );
    assert!(
        result.updated.is_empty(),
        "Calendar class must not fill updated"
    );

    // Add → calendar_added with ServerId + full props.
    assert_eq!(result.calendar_added.len(), 1, "exactly one Add command");
    let added = &result.calendar_added[0];
    assert_eq!(added.server_id, "cal:1");
    assert_eq!(
        added.props,
        expected_full_props(),
        "full Task-2/3 props fidelity through the Sync envelope"
    );

    // Change → calendar_updated with ServerId + the all-day props.
    assert_eq!(
        result.calendar_updated.len(),
        1,
        "exactly one Change command"
    );
    let updated = &result.calendar_updated[0];
    assert_eq!(updated.server_id, "cal:2");
    assert!(updated.props.all_day_event);
    assert_eq!(updated.props.subject.as_deref(), Some("Company Holiday"));
    assert_eq!(
        updated.props.start_time.as_deref(),
        Some("20260820T000000Z")
    );
    assert_eq!(updated.props.end_time.as_deref(), Some("20260821T000000Z"));
    assert_eq!(updated.props.busy_status, Some(0));

    // Delete → the shared, class-agnostic deleted_server_ids.
    assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
}

/// Brief test (2): the SAME response under class "Email" keeps today's
/// Email-shaped behavior bit-for-bit — calendar vectors empty, items in
/// `added` / `updated` via the tag_name-dispatching Email parser (which
/// picks up the page-4 `Subject` token collision and the AirSyncBase
/// Type-1 Body, and ignores the rest).
#[test]
fn email_class_sync_keeps_email_shaped_parse() {
    let tree = fixture_calendar_sync_response();

    let result = parse_sync_response_for_class(&tree, "Email").expect("parse");

    assert!(
        result.calendar_added.is_empty(),
        "Email class must not fill calendar_added"
    );
    assert!(
        result.calendar_updated.is_empty(),
        "Email class must not fill calendar_updated"
    );

    // Email-shaped items, unchanged from the pre-M8 parse path.
    assert_eq!(result.added.len(), 1);
    assert_eq!(result.added[0].server_id, "cal:1");
    assert_eq!(result.added[0].subject.as_deref(), Some("Weekly Sync"));
    assert_eq!(
        result.added[0].body_text.as_deref(),
        Some("Agenda: sync status"),
        "AirSyncBase Body Type=1 lands in body_text on the Email path"
    );
    assert_eq!(result.added[0].body_html, None);
    assert_eq!(result.added[0].from, None);

    assert_eq!(result.updated.len(), 1);
    assert_eq!(result.updated[0].server_id, "cal:2");
    assert_eq!(
        result.updated[0].subject.as_deref(),
        Some("Company Holiday")
    );

    assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
}

/// Brief test (2b): an EMPTY class — the pre-M8 construction default —
/// behaves exactly like "Email" (old behavior bit-for-bit).
#[test]
fn empty_class_sync_defaults_to_email_shaped_parse() {
    let tree = fixture_calendar_sync_response();

    let result = parse_sync_response_for_class(&tree, "").expect("parse");

    assert!(result.calendar_added.is_empty());
    assert!(result.calendar_updated.is_empty());
    assert_eq!(result.added.len(), 1);
    assert_eq!(result.added[0].server_id, "cal:1");
    assert_eq!(result.updated.len(), 1);
    assert_eq!(result.deleted_server_ids, vec!["cal:3".to_string()]);
}

/// Brief test (3): sync_key / more_available / status parse identically
/// for both classes, and the legacy class-unaware entry matches the
/// Email-class entry.
#[test]
fn sync_envelope_fields_identical_across_classes() {
    let tree = fixture_calendar_sync_response();

    let calendar = parse_sync_response_for_class(&tree, "Calendar").expect("parse");
    let email = parse_sync_response_for_class(&tree, "Email").expect("parse");
    let legacy = parse_sync_response(&tree).expect("parse");

    assert_eq!(calendar.sync_key, email.sync_key);
    assert_eq!(calendar.status, email.status);
    assert_eq!(calendar.more_available, email.more_available);
    assert_eq!(calendar.deleted_server_ids, email.deleted_server_ids);

    // The class-unaware entry is the Email-shaped entry.
    assert_eq!(legacy.sync_key, email.sync_key);
    assert_eq!(legacy.status, email.status);
    assert_eq!(legacy.added.len(), email.added.len());
    assert_eq!(legacy.updated.len(), email.updated.len());
}

// ========================================================================
// Tests — M8-C task 1: Contacts-class SyncResult seam
// ========================================================================

/// Minimal Contacts ApplicationData for the Change item: FileAs only.
/// Deliberately DIFFERENT from the full fixture so a crossed wire
/// between the Add and Change items cannot hide behind equal props.
fn file_as_only_app_data() -> WbxmlElement {
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_APPLICATION_DATA,
        vec![WbxmlElement::text(
            PAGE_CONTACTS,
            CON_FILE_AS,
            "Kerry, Anat",
        )],
    )
}

/// Contacts-class Sync response fixture: Collection with SyncKey
/// "{con-sk1}", Status "1", MoreAvailable, and a Commands block with
/// one Add (ServerId "con:1" + the full C1 ApplicationData fixture),
/// one Change (ServerId "con:2" + the FileAs-only item), one Delete
/// (ServerId "con:3" — deletes are class-agnostic on the wire).
fn fixture_contacts_sync_response() -> WbxmlElement {
    let add = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_ADD,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:1"),
            fixture_full_contact_app_data(),
        ],
    );
    let change = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_CHANGE,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:2"),
            file_as_only_app_data(),
        ],
    );
    // MS-ASCMD 2.2.3.42.2: Delete is a CONTAINER with the ServerId as
    // a child — same envelope shape as Add/Change.
    let delete = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_DELETE,
        vec![WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, "con:3")],
    );
    let commands = WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, vec![add, change, delete]);
    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, "{con-sk1}"),
            WbxmlElement::text(PAGE_AIRSYNC, AS_STATUS, "1"),
            WbxmlElement::empty(PAGE_AIRSYNC, AS_MORE_AVAILABLE),
            commands,
        ],
    );
    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Brief test: a Contacts-class response routes Add/Change through the
/// MS-ASCNTC parser — `contacts_added` / `contacts_updated` carry the
/// ServerIds + FULL props, deletes land in the shared
/// `deleted_server_ids`, and BOTH the Email-shaped `added` / `updated`
/// and the Calendar vectors stay EMPTY (no double delivery — the
/// regression pins for the other two classes).
#[test]
fn contacts_class_sync_routes_items_to_contacts_vectors() {
    let tree = fixture_contacts_sync_response();

    let result = parse_sync_response_for_class(&tree, "Contacts").expect("parse");

    // Envelope fields parse identically for the Contacts class.
    assert_eq!(result.sync_key, "{con-sk1}");
    assert_eq!(result.status, 1);
    assert!(result.more_available);

    // Email-shaped vectors stay empty — no double delivery.
    assert!(
        result.added.is_empty(),
        "Contacts class must not fill added"
    );
    assert!(
        result.updated.is_empty(),
        "Contacts class must not fill updated"
    );
    // Calendar vectors stay empty too — class routing is exclusive.
    assert!(
        result.calendar_added.is_empty(),
        "Contacts class must not fill calendar_added"
    );
    assert!(
        result.calendar_updated.is_empty(),
        "Contacts class must not fill calendar_updated"
    );

    // Add → contacts_added with ServerId + full C1 props.
    assert_eq!(result.contacts_added.len(), 1, "exactly one Add command");
    let added = &result.contacts_added[0];
    assert_eq!(added.server_id, "con:1");
    assert_eq!(
        added.props,
        expected_full_contact_props(),
        "full C1 props fidelity through the Sync envelope"
    );

    // Change → contacts_updated with ServerId + the FileAs-only props.
    assert_eq!(
        result.contacts_updated.len(),
        1,
        "exactly one Change command"
    );
    let updated = &result.contacts_updated[0];
    assert_eq!(updated.server_id, "con:2");
    assert_eq!(
        updated.props,
        ContactsContactProps {
            file_as: Some("Kerry, Anat".to_string()),
            ..Default::default()
        },
        "everything but FileAs stays None on the minimal item"
    );

    // Delete → the shared, class-agnostic deleted_server_ids.
    assert_eq!(result.deleted_server_ids, vec!["con:3".to_string()]);
}

/// Brief test: the SAME Contacts-class response under class "Email"
/// keeps today's Email-shaped behavior bit-for-bit — contacts/calendar
/// vectors empty, items in `added` / `updated` via the tag_name-
/// dispatching Email parser (which ignores the page-1 contacts tokens
/// and picks up only the AirSyncBase Type-1 Body).
#[test]
fn email_class_sync_keeps_email_shaped_parse_for_contacts_fixture() {
    let tree = fixture_contacts_sync_response();

    let result = parse_sync_response_for_class(&tree, "Email").expect("parse");

    assert!(
        result.contacts_added.is_empty(),
        "Email class must not fill contacts_added"
    );
    assert!(
        result.contacts_updated.is_empty(),
        "Email class must not fill contacts_updated"
    );
    assert!(result.calendar_added.is_empty());
    assert!(result.calendar_updated.is_empty());

    // Email-shaped items, unchanged from the pre-M8 parse path.
    assert_eq!(result.added.len(), 1);
    assert_eq!(result.added[0].server_id, "con:1");
    assert_eq!(
        result.added[0].body_text.as_deref(),
        Some("Prefers plain-text bodies."),
        "AirSyncBase Body Type=1 lands in body_text on the Email path"
    );
    assert_eq!(
        result.added[0].subject, None,
        "page-1 contacts tokens are invisible to the Email parser"
    );
    assert_eq!(result.added[0].from, None);

    assert_eq!(result.updated.len(), 1);
    assert_eq!(result.updated[0].server_id, "con:2");
    assert_eq!(result.updated[0].subject, None);
    assert_eq!(result.updated[0].body_text, None);

    assert_eq!(result.deleted_server_ids, vec!["con:3".to_string()]);
}
