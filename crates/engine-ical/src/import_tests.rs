use super::*;

/// A meeting with an override, an alarm that mails an attendee, and a `METHOD` an exported
/// `.ics` file carries: every shape the preparation has to tell apart.
const MEETING: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nMETHOD:REQUEST\r\n\
BEGIN:VEVENT\r\nUID:series@example.com\r\nDTSTAMP:20260101T000000Z\r\n\
DTSTART:20260105T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Standup\r\n\
ORGANIZER;CN=Bob:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:carol@example.com\r\n\
BEGIN:VALARM\r\nACTION:EMAIL\r\nATTENDEE:mailto:bob@example.com\r\nSUMMARY:x\r\n\
DESCRIPTION:y\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:series@example.com\r\nDTSTAMP:20260101T000000Z\r\n\
RECURRENCE-ID:20260106T100000Z\r\nDTSTART:20260106T110000Z\r\nSUMMARY:Moved\r\n\
ORGANIZER;SCHEDULE-AGENT=SERVER;SCHEDULE-FORCE-SEND=REQUEST:mailto:bob@example.com\r\n\
ATTENDEE;SCHEDULE-AGENT=NONE:mailto:carol@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn prepared(text: &str) -> ImportObject {
    import_object(text).expect("an importable object")
}

/// `ical` with its folds undone (RFC 5545 §3.1), so a rewritten line reads whole.
fn unfolded(ical: &RawIcal) -> String {
    ical.as_str().replace("\r\n ", "")
}

/// The lines of `ical` whose property is `name`, unfolded.
fn lines<'a>(ical: &'a str, name: &str) -> Vec<&'a str> {
    ical.split("\r\n")
        .filter(|line| {
            line.split([';', ':'])
                .next()
                .is_some_and(|property| property.eq_ignore_ascii_case(name))
        })
        .collect()
}

#[test]
fn every_organizer_and_attendee_is_left_to_the_client() {
    let ical = unfolded(&prepared(MEETING).ical);
    let ical = ical.as_str();
    let parties: Vec<&str> = lines(ical, "ORGANIZER")
        .into_iter()
        .chain(lines(ical, "ATTENDEE"))
        .collect();
    // Two organizers and three attendees: the master's, the override's, and the alarm's.
    assert_eq!(parties.len(), 5);
    let scheduled: Vec<&&str> = parties
        .iter()
        .filter(|line| line.contains("SCHEDULE-AGENT=CLIENT"))
        .collect();
    assert_eq!(
        scheduled.len(),
        4,
        "the master's and the override's organizer and attendee: {ical}"
    );
}

#[test]
fn an_override_that_asked_the_server_to_schedule_no_longer_does() {
    let ical = prepared(MEETING).ical;
    let ical = ical.as_str();
    assert!(!ical.contains("SCHEDULE-AGENT=SERVER"));
    assert!(!ical.contains("SCHEDULE-AGENT=NONE"));
    assert!(
        !ical.contains("SCHEDULE-FORCE-SEND"),
        "a send forced by the source is a request to the server, not data"
    );
}

#[test]
fn an_alarm_recipient_is_not_a_scheduling_party() {
    let ical = prepared(MEETING).ical;
    assert!(
        ical.as_str()
            .contains("ACTION:EMAIL\r\nATTENDEE:mailto:bob@example.com\r\n"),
        "a VALARM's ATTENDEE names who the reminder mails, which no server schedules"
    );
}

#[test]
fn the_parameters_the_source_had_are_kept() {
    let ical = unfolded(&prepared(MEETING).ical);
    assert!(ical.contains(
        "ATTENDEE;CN=Carol;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0;SCHEDULE-AGENT=CLIENT:mailto:\
         carol@example.com"
    ));
}

#[test]
fn a_method_is_not_stored() {
    // RFC 4791 §4.1: a calendar object resource MUST NOT carry METHOD.
    let ical = prepared(MEETING).ical;
    assert_eq!(lines(ical.as_str(), "METHOD"), Vec::<&str>::new());
}

#[test]
fn the_uid_is_the_resources() {
    assert_eq!(prepared(MEETING).uid.as_str(), "series@example.com");
}

#[test]
fn every_other_byte_is_kept() {
    // Strike the rewritten lines from both documents and the rest is byte-equal.
    let ical = unfolded(&prepared(MEETING).ical);
    let strip = |text: &str| -> String {
        text.split("\r\n")
            .filter(|line| {
                !(line.starts_with("ORGANIZER")
                    || line.starts_with("ATTENDEE")
                    || line.starts_with("METHOD"))
            })
            .collect::<Vec<_>>()
            .join("\r\n")
    };
    assert_eq!(strip(&ical), strip(MEETING));
}

#[test]
fn a_line_already_left_to_the_client_is_untouched() {
    let text = MEETING.replace(
        "ORGANIZER;CN=Bob:mailto:bob@example.com",
        "ORGANIZER;CN=Bob;SCHEDULE-AGENT=CLIENT:mailto:bob@example.com",
    );
    let ical = prepared(&text).ical;
    assert!(
        ical.as_str()
            .contains("ORGANIZER;CN=Bob;SCHEDULE-AGENT=CLIENT:mailto:bob@example.com\r\n")
    );
}

#[test]
fn a_long_party_line_is_folded() {
    let long = format!("ATTENDEE;CN={}:mailto:carol@example.com", "C".repeat(80));
    let text = MEETING.replace(
        "ATTENDEE;CN=Carol;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:carol@example.com",
        &long,
    );
    let ical = prepared(&text).ical;
    assert!(
        ical.as_str().split("\r\n").all(|line| line.len() <= 75),
        "every content line is folded at 75 octets (RFC 5545 §3.1)"
    );
    assert!(ical.as_str().replace("\r\n ", "").contains(&format!(
        "ATTENDEE;CN={};SCHEDULE-AGENT=CLIENT:mailto:carol@example.com",
        "C".repeat(80)
    )));
}

#[test]
fn a_quoted_parameter_holding_a_colon_or_semicolon_survives() {
    let text = MEETING.replace(
        "ORGANIZER;CN=Bob:mailto:bob@example.com",
        "ORGANIZER;CN=\"Bob; the boss: really\":mailto:bob@example.com",
    );
    let ical = unfolded(&prepared(&text).ical);
    assert!(ical.contains(
        "ORGANIZER;CN=\"Bob; the boss: really\";SCHEDULE-AGENT=CLIENT:mailto:bob@example.com"
    ));
}

#[test]
fn an_object_without_an_event_is_refused() {
    let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:t@example.com\r\n\
                END:VTODO\r\nEND:VCALENDAR\r\n";
    assert!(import_object(todo).is_err());
    assert!(import_object("not a calendar").is_err());
}

#[test]
fn an_object_holding_two_events_is_refused() {
    // RFC 4791 §4.1: one UID per resource. A file of several events is split first.
    let two = MEETING
        .replace("RECURRENCE-ID:20260106T100000Z\r\n", "")
        .replacen("UID:series@example.com", "UID:other@example.com", 2)
        .replacen("UID:other@example.com", "UID:series@example.com", 1);
    assert!(import_object(&two).is_err());
}

#[test]
fn a_bare_lf_document_keeps_its_terminators() {
    let text = MEETING.replace("\r\n", "\n");
    let ical = prepared(&text).ical;
    assert!(!ical.as_str().contains('\r'));
    assert!(
        ical.as_str()
            .contains("ORGANIZER;CN=Bob;SCHEDULE-AGENT=CLIENT:mailto:bob@example.com\n")
    );
}

/// What fails closed: a party the rewrite cannot be sure of is a refused object, never one
/// stored as it was. From an independent review (S1–S5).
mod fails_closed {
    use super::*;

    fn wrap(body: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u@example.com\r\n\
             DTSTART:20260105T100000Z\r\n{body}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// Every `ORGANIZER`/`ATTENDEE` line of `ical`, unfolded, with how many `SCHEDULE-AGENT`s it
    /// carries and whether one says `CLIENT`.
    fn agents(ical: &RawIcal) -> Vec<(usize, bool, bool)> {
        unfolded(ical)
            .split("\r\n")
            .filter(|line| {
                let name = line
                    .split([';', ':'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                name.ends_with("ATTENDEE") || name.ends_with("ORGANIZER")
            })
            .map(|line| {
                let head = line.split(':').next().unwrap().to_ascii_uppercase();
                let count = head.matches("SCHEDULE-AGENT").count();
                (
                    count,
                    head.contains("SCHEDULE-AGENT=CLIENT")
                        || head.contains("SCHEDULE-AGENT=\"CLIENT\""),
                    head.contains("FORCE-SEND"),
                )
            })
            .collect()
    }

    #[test]
    fn s1_a_party_with_an_unbalanced_quote_is_refused() {
        let text = wrap(
            "ORGANIZER:mailto:bob@example.com\r\n\
             ATTENDEE;CN=Bob \"the builder:mailto:carol@example.com\r\n",
        );
        assert!(import_object(&text).is_err());
    }

    #[test]
    fn s1_a_party_line_without_a_value_is_refused() {
        assert!(import_object(&wrap("ATTENDEE;CN=Carol\r\n")).is_err());
    }

    #[test]
    fn s2_components_that_do_not_nest_are_refused() {
        let unclosed = wrap(
            "BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT5M\r\n\
             ORGANIZER:mailto:bob@example.com\r\nATTENDEE:mailto:carol@example.com\r\n",
        );
        assert!(import_object(&unclosed).is_err());
        let crossed = wrap("BEGIN:VALARM\r\nTRIGGER:-PT5M\r\n")
            .replace("END:VEVENT\r\n", "END:VEVENT\r\nEND:VALARM\r\n");
        assert!(import_object(&crossed).is_err());
    }

    #[test]
    fn s3_every_spelling_of_a_parameter_ends_as_one_client_agent() {
        let text = wrap(
            "ORGANIZER:mailto:bob@example.com\r\n\
             ATTENDEE; SCHEDULE-AGENT=SERVER:mailto:carol@example.com\r\n\
             ATTENDEE;SCHEDULE-AGENT=\"CLIENT\":mailto:dan@example.com\r\n\
             ATTENDEE;schedule-agent=client;SCHEDULE-FORCE-SEND=REQUEST:mailto:eve@example.com\r\n\
             ATTENDEE;SCHEDULE-AGENT=CLIENT;SCHEDULE-AGENT=SERVER:mailto:fay@example.com\r\n",
        );
        let parties = agents(&prepared(&text).ical);
        assert_eq!(parties.len(), 5);
        for (count, client, forced) in parties {
            assert_eq!((count, client, forced), (1, true, false));
        }
    }

    #[test]
    fn s4_an_object_holding_a_task_beside_the_event_is_refused() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u@example.com\r\n\
                    ORGANIZER:mailto:bob@example.com\r\nEND:VEVENT\r\nBEGIN:VTODO\r\n\
                    UID:u@example.com\r\nATTENDEE:mailto:carol@example.com\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        assert!(import_object(text).is_err());
    }

    #[test]
    fn s5_an_event_nested_deeper_is_refused() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u@example.com\r\nEND:VEVENT\r\n\
                    BEGIN:X-WRAP\r\nBEGIN:VEVENT\r\nUID:u@example.com\r\n\
                    ATTENDEE:mailto:carol@example.com\r\nEND:VEVENT\r\nEND:X-WRAP\r\n\
                    END:VCALENDAR\r\n";
        assert!(import_object(text).is_err());
    }

    #[test]
    fn a_group_prefixed_party_is_left_to_the_client_too() {
        let text =
            wrap("ORGANIZER:mailto:bob@example.com\r\nA.ATTENDEE:mailto:carol@example.com\r\n");
        let ical = unfolded(&prepared(&text).ical);
        assert!(
            ical.contains("A.ATTENDEE;SCHEDULE-AGENT=CLIENT:mailto:carol@example.com"),
            "{ical}"
        );
    }

    #[test]
    fn a_property_outside_every_component_is_refused() {
        let text = format!("ATTENDEE:mailto:carol@example.com\r\n{}", wrap(""));
        assert!(import_object(&text).is_err());
    }

    #[test]
    fn the_uid_is_read_without_rewriting_anything() {
        assert_eq!(object_uid(MEETING).unwrap().as_str(), "series@example.com");
        let todo = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t@example.com\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        assert!(object_uid(todo).is_err(), "a task is not an event");
    }
}
