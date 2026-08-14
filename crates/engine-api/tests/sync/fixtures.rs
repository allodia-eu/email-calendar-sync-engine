//! Shared object fixtures for the sync suite: the accounts, mailboxes, messages, calendars and
//! events every case builds on.
//!
//! Split out of `sync.rs` to keep both files inside the 500-line limit. Reached through
//! `#[path]` from the crate root, because a `tests/NAME.rs` *is* a crate root and a nested file
//! is not a module of it by default.

use super::*;

pub(crate) fn account() -> AccountId {
    AccountId::try_from("acct-1").expect("valid account")
}

pub(crate) fn mailbox(id: &str, name: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), name);
    mailbox.role = role;
    mailbox
}

pub(crate) fn message(id: &str, mailbox: &str, subject: &str) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.envelope.subject = Some(subject.to_owned());
    message
}

pub(crate) fn threaded_message(id: &str, mailbox: &str, own: &str, references: &[&str]) -> Message {
    let mut message = message(id, mailbox, "subject");
    message.envelope.message_id = vec![MessageIdHeader::new(own).unwrap()];
    message.envelope.references = references
        .iter()
        .map(|value| MessageIdHeader::new(*value).unwrap())
        .collect();
    message
}

/// An inbox message with a delivery date and threading headers, for the windowed and thread
/// reads (its `received_at` becomes the mail index's sort date).
pub(crate) fn dated_message(id: &str, own: &str, references: &[&str], received: &str) -> Message {
    let mut message = threaded_message(id, "a", own, references);
    message.received_at = Some(received.parse().unwrap());
    message
}

pub(crate) fn calendar(id: &str, name: &str) -> Calendar {
    Calendar::new(CalendarId::try_from(id).unwrap(), name)
}

pub(crate) fn event(id: &str, uid: &str, calendar: &str) -> Event {
    Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from(calendar).unwrap()),
        CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
    )
}

/// A weekly standup recurring `count` times from `start` (a UTC wall clock) — the
/// event whose instances only exist in the occurrence rows, never in `events()`.
pub(crate) fn weekly_event(id: &str, uid: &str, start: LocalDateTime, count: u32) -> Event {
    let mut event = Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        CalendarDateTime::utc(start),
    );
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(count).unwrap());
    event.recurrence = Some(Recurrence::from_rule(rule));
    event
}

pub(crate) fn horizon() -> Horizon {
    Horizon::new(
        "2020-01-01T00:00:00Z".parse().unwrap(),
        "2030-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

pub(crate) fn draft(message_id: &str, subject: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        subject,
        "see attached",
    )
}
