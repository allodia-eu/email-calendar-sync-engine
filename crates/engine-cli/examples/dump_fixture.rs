//! Prints a sample `engine-cli` ingest fixture (a daily recurring event plus a
//! message) as JSON. Fixtures are JSON of normalized engine-core objects, so the
//! easiest way to author one is to serialize real objects.
//!
//! Regenerate the committed sample with:
//! `cargo run -p engine-cli --example dump_fixture > crates/engine-cli/fixtures/sample.json`

use engine_core::calendar::{Event, Frequency, Recurrence, RecurrenceRule};
use engine_core::ids::{CalendarId, EventId, MailboxId, MessageId, Uid};
use engine_core::mail::{EmailAddress, Message};
use engine_core::membership::Memberships;
use engine_core::time::{CalendarDateTime, LocalDateTime};

fn main() {
    let mut event = Event::new(
        EventId::try_from("daily-standup").expect("event id"),
        Uid::new("uid-standup").expect("uid"),
        Memberships::of_one(CalendarId::try_from("work").expect("calendar id")),
        CalendarDateTime::Zoned {
            local: LocalDateTime::new(2026, 6, 1, 9, 0, 0).expect("local date-time"),
            zone: engine_core::time::TimeZoneId::iana("Europe/Amsterdam").expect("zone"),
        },
    );
    event.title = String::from("Daily standup");
    event.duration = "PT15M".parse().expect("duration");
    event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));

    let mut message = Message::new(
        MessageId::try_from("welcome").expect("message id"),
        Memberships::of_one(MailboxId::try_from("inbox").expect("mailbox id")),
    );
    message.envelope.subject = Some("Welcome to the engine".to_owned());
    message.envelope.from = vec![EmailAddress::new("team@example.com")];

    let fixture = serde_json::json!({ "events": [event], "messages": [message] });
    println!(
        "{}",
        serde_json::to_string_pretty(&fixture).expect("serialize fixture")
    );
}
