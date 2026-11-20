use super::*;

fn since(month: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap())
}

fn years_back(year: i32) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, 1, 1).unwrap())
}

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

fn account(window: SyncWindow) -> MailboxWindows {
    MailboxWindows::new(window)
}

fn resumed(stored: &SyncState, depth: &MailboxWindows) -> Option<String> {
    resume(Some(stored.clone()), depth).map(|c| c.as_str().to_owned())
}

#[test]
fn a_full_window_stores_the_cursor_bare() {
    let stored = stamp(SyncState::new("c1"), &account(SyncWindow::full()));
    assert_eq!(stored.as_str(), "c1");
    assert_eq!(
        resumed(&stored, &account(SyncWindow::full())).as_deref(),
        Some("c1")
    );
    assert_eq!(resumed(&stored, &account(since(4))).as_deref(), Some("c1"));
    let deepened = account(since(4)).deepen(mailbox("sent"), years_back(2020));
    assert_eq!(resumed(&stored, &deepened).as_deref(), Some("c1"));
}

#[test]
fn a_floor_resumes_for_itself_and_later_floors_only() {
    let stored = stamp(SyncState::new("c1"), &account(since(4)));
    assert_eq!(stored.as_str(), "since=2026-04-01\nc1");
    assert_eq!(resumed(&stored, &account(since(4))).as_deref(), Some("c1"));
    assert_eq!(resumed(&stored, &account(since(5))).as_deref(), Some("c1"));
    assert_eq!(resumed(&stored, &account(since(3))), None);
    assert_eq!(resumed(&stored, &account(SyncWindow::full())), None);
}

#[test]
fn deepening_a_mailbox_starts_the_scope_over() {
    let stored = stamp(SyncState::new("c1"), &account(since(4)));
    let deepened = account(since(4)).deepen(mailbox("sent"), years_back(2020));
    assert_eq!(resumed(&stored, &deepened), None);
    let whole = account(since(4)).deepen(mailbox("sent"), SyncWindow::full());
    assert_eq!(resumed(&stored, &whole), None);
    // An override that reaches no further than the account's window deepens nothing.
    let shallow = account(since(4)).deepen(mailbox("sent"), since(6));
    assert_eq!(resumed(&stored, &shallow).as_deref(), Some("c1"));
}

#[test]
fn a_deepened_mailbox_is_recorded_and_resumes_for_itself_and_later_floors() {
    let depth = account(since(4))
        .deepen(mailbox("sent"), years_back(2020))
        .deepen(mailbox("archive"), SyncWindow::full());
    let stored = stamp(SyncState::new("c1"), &depth);
    assert_eq!(
        stored.as_str(),
        "since=2026-04-01\ndeeper=* archive\ndeeper=2020-01-01 sent\nc1"
    );
    assert_eq!(resumed(&stored, &depth).as_deref(), Some("c1"));

    // Sent held less far back, or no longer at all, resumes: nothing more is wanted.
    let later = account(since(5))
        .deepen(mailbox("sent"), years_back(2022))
        .deepen(mailbox("archive"), SyncWindow::full());
    assert_eq!(resumed(&stored, &later).as_deref(), Some("c1"));
    assert_eq!(resumed(&stored, &account(since(4))).as_deref(), Some("c1"));

    // Sent held further back, or a mailbox newly deepened, starts over.
    let further = depth.clone().deepen(mailbox("sent"), years_back(2019));
    assert_eq!(resumed(&stored, &further), None);
    let another = depth.clone().deepen(mailbox("inbox"), years_back(2025));
    assert_eq!(resumed(&stored, &another), None);
    // So does the account reaching back past its own floor, whatever Sent held.
    let wider = account(since(3))
        .deepen(mailbox("sent"), years_back(2020))
        .deepen(mailbox("archive"), SyncWindow::full());
    assert_eq!(resumed(&stored, &wider), None);
}

#[test]
fn a_mailbox_id_with_a_line_break_or_backslash_survives_the_record() {
    let odd = mailbox("a\\b\nc d");
    let depth = account(since(4)).deepen(odd.clone(), years_back(2020));
    let stored = stamp(SyncState::new("c1"), &depth);
    assert_eq!(resumed(&stored, &depth).as_deref(), Some("c1"));
    let further = account(since(4)).deepen(odd, years_back(2019));
    assert_eq!(resumed(&stored, &further), None);
}

#[test]
fn a_provider_cursor_that_only_looks_stamped_is_kept_whole() {
    for raw in [
        "since=",
        "since=soon\nc1",
        "since=2026-04-01",
        "since=2026-04-01\ndeeper=2020-01-01\nc1",
        "since=2026-04-01\ndeeper=never sent\nc1",
        "since=2026-04-01\ndeeper=* bad\\escape\nc1",
    ] {
        let stored = SyncState::new(raw);
        assert_eq!(
            resumed(&stored, &account(SyncWindow::full())).as_deref(),
            Some(raw)
        );
    }
}
