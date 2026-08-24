//! Unit tests for [`Capabilities`](super::Capabilities) and the two promises it
//! carries beside a plain flag ([`WriteGuard`](super::WriteGuard),
//! [`RsvpControls`](super::RsvpControls)). A sibling file so `capability.rs` stays
//! under the line limit.

use super::*;

#[test]
fn builder_sets_each_flag_independently() {
    assert_eq!(Capabilities::none(), Capabilities::default());
    let caps = Capabilities::none().with_mail().with_calendars();
    assert!(caps.mail());
    assert!(caps.calendars());
    assert!(!caps.submission());
    assert!(!caps.calendar_writes());
    assert!(!caps.mail_writes());
}

#[test]
fn full_capability_set() {
    let caps = Capabilities::none()
        .with_mail()
        .with_mail_writes()
        .with_message_source()
        .with_submission()
        .with_idle()
        .with_calendars()
        .with_calendar_writes(WriteGuard::Enforced, OverrideSurvival::kept())
        .with_calendar_scheduling()
        .with_contacts()
        .with_contact_writes(WriteGuard::Enforced)
        .with_contact_groups()
        .with_contact_photos();
    assert!(caps.mail() && caps.mail_writes() && caps.submission());
    assert!(caps.message_source() && caps.idle());
    assert!(caps.calendars() && caps.calendar_writes());
    assert_eq!(caps.calendar_write_guard(), Some(WriteGuard::Enforced));
    assert!(caps.calendar_scheduling());
    assert!(caps.contacts() && caps.contact_writes());
    assert!(caps.contact_groups() && caps.contact_photos());
    assert_eq!(caps.contact_write_guard(), Some(WriteGuard::Enforced));
}

#[test]
fn answering_an_invitation_and_anyone_hearing_it_are_separate_promises() {
    // `calendar_rsvp` says the transport can express an answer; `calendar_scheduling` says
    // somebody is told. On CalDAV those come apart: RFC 4791 is calendar *access*, and a
    // server without RFC 6638 stores the rewritten PARTSTAT and schedules nothing. An
    // adapter that reported the first as if it implied the second would hand a host the
    // silent success `RsvpControls` exists to prevent — so the two are distinct flags and
    // "can answer, nobody hears" is representable.
    let plain_caldav = Capabilities::none()
        .with_calendars()
        .with_calendar_writes(WriteGuard::Enforced, OverrideSurvival::kept())
        .with_calendar_rsvp(RsvpControls {
            comment: false,
            suppress_notification: false,
            guard: WriteGuard::Enforced,
        });
    assert!(plain_caldav.calendar_rsvp().is_some());
    assert!(
        !plain_caldav.calendar_scheduling(),
        "a transport says nothing about scheduling until it discovers it"
    );

    let auto_schedule = plain_caldav.with_calendar_scheduling();
    assert!(auto_schedule.calendar_rsvp().is_some());
    assert!(auto_schedule.calendar_scheduling());
}

#[test]
fn scheduling_is_independent_of_every_other_calendar_flag() {
    // Deliberately not folded into `calendar_writes` or `calendar_rsvp`: a read-only
    // calendar on an auto-scheduling server still schedules for the collections the
    // account *can* write, and a writable one on a plain server never does. Neither
    // implies the other, so neither may carry the other's value.
    let read_only_but_scheduling = Capabilities::none()
        .with_calendars()
        .with_calendar_scheduling();
    assert!(read_only_but_scheduling.calendar_scheduling());
    assert!(!read_only_but_scheduling.calendar_writes());
    assert!(read_only_but_scheduling.calendar_rsvp().is_none());
}

#[test]
fn a_writable_calendar_states_how_strong_its_guard_is() {
    // "Can write" and "can refuse a stale write" are different promises, and a caller
    // that conflates them silently clobbers concurrent edits on the transports where
    // only the first holds. So the write capability *is* the guard strength — a
    // writable-but-unguarded adapter (JMAP) is representable and says so.
    let caldav = Capabilities::none()
        .with_calendars()
        .with_calendar_writes(WriteGuard::Enforced, OverrideSurvival::kept());
    let jmap = Capabilities::none()
        .with_calendars()
        .with_calendar_writes(WriteGuard::Absent, OverrideSurvival::kept());

    assert!(caldav.calendar_writes() && jmap.calendar_writes());
    assert_eq!(caldav.calendar_write_guard(), Some(WriteGuard::Enforced));
    assert_eq!(jmap.calendar_write_guard(), Some(WriteGuard::Absent));

    // And a read-only calendar has no guard to report, because it has no write.
    let read_only = Capabilities::none().with_calendars();
    assert_eq!(read_only.calendar_write_guard(), None);
}

#[test]
fn idle_is_independent_of_read() {
    // An adapter can read/sync mail without offering push (a server without IMAP
    // `IDLE`), exactly as a read-only mailbox advertises `mail` without
    // `mail_writes`. Push is a latency optimization layered on top of sync.
    let poll_only = Capabilities::none().with_mail();
    assert!(poll_only.mail() && !poll_only.idle());
    let pushable = Capabilities::none().with_mail().with_idle();
    assert!(pushable.mail() && pushable.idle());
}

#[test]
fn message_source_is_independent_of_read() {
    // An adapter can sync envelope metadata without supporting full-body fetch,
    // exactly as a read-only mailbox advertises `mail` without `mail_writes`.
    let metadata_only = Capabilities::none().with_mail();
    assert!(metadata_only.mail() && !metadata_only.message_source());
}

#[test]
fn calendar_writes_is_independent_of_read() {
    // A read-only calendar advertises `calendars` without `calendar_writes`,
    // exactly as a no-SMTP mail adapter advertises `mail` without `submission`.
    let read_only = Capabilities::none().with_calendars();
    assert!(read_only.calendars() && !read_only.calendar_writes());
}

#[test]
fn mail_writes_is_independent_of_read() {
    // A read-only mailbox advertises `mail` without `mail_writes`, exactly as a
    // read-only calendar advertises `calendars` without `calendar_writes`.
    let read_only = Capabilities::none().with_mail();
    assert!(read_only.mail() && !read_only.mail_writes());
}
