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

fn read(raw: &str) -> StampedCursor {
    StampedCursor::parse(SyncState::new(raw))
}

#[test]
fn a_cursor_over_the_whole_history_is_stored_bare() {
    let stamped = StampedCursor::new(SyncState::new("c1"), MailboxWindows::default());
    assert_eq!(stamped.to_state().as_str(), "c1");
    assert_eq!(read("c1"), stamped);
    assert_eq!(read("c1").covered(), &MailboxWindows::default());
    assert_eq!(read("c1").target(), None);
}

#[test]
fn every_stamp_written_before_targets_reads_to_the_same_depth() {
    // The forms `engine-sync`'s window_cursor wrote before the record moved here.
    let plain = read("since=2026-04-01\nc1");
    assert_eq!(plain.covered(), &account(since(4)));
    assert_eq!(plain.target(), None);
    assert_eq!(plain.cursor().as_str(), "c1");

    let deepened = read("since=2026-04-01\ndeeper=* archive\ndeeper=2020-01-01 sent\nc1");
    assert_eq!(
        deepened.covered(),
        &account(since(4))
            .deepen(mailbox("sent"), years_back(2020))
            .deepen(mailbox("archive"), SyncWindow::full())
    );
    assert_eq!(deepened.target(), None);
    assert_eq!(deepened.cursor().as_str(), "c1");
}

#[test]
fn a_covered_depth_is_written_in_the_form_it_always_was() {
    let depth = account(since(4))
        .deepen(mailbox("sent"), years_back(2020))
        .deepen(mailbox("archive"), SyncWindow::full());
    let stamped = StampedCursor::new(SyncState::new("c1"), depth.clone());
    assert_eq!(
        stamped.to_state().as_str(),
        "since=2026-04-01\ndeeper=* archive\ndeeper=2020-01-01 sent\nc1"
    );
    assert_eq!(StampedCursor::parse(stamped.to_state()), stamped);
}

#[test]
fn a_target_is_recorded_beside_the_covered_depth_and_read_back() {
    let target = account(years_back(2024)).deepen(mailbox("sent"), SyncWindow::full());
    let stamped =
        StampedCursor::new(SyncState::new("c1"), account(since(4))).widening_to(target.clone());
    assert_eq!(
        stamped.to_state().as_str(),
        "since=2026-04-01\ndeeper=2024-01-01 \\ntarget\ndeeper=* \\ntarget sent\nc1"
    );
    let back = StampedCursor::parse(stamped.to_state());
    assert_eq!(back, stamped);
    assert_eq!(back.covered(), &account(since(4)));
    assert_eq!(back.target(), Some(&target));
    assert!(!back.is_band_begun());
    assert_eq!(back.into_cursor().as_str(), "c1");

    let begun = StampedCursor::new(SyncState::new("c1"), account(since(4)))
        .widening_to(MailboxWindows::default())
        .band_begun();
    assert_eq!(
        begun.to_state().as_str(),
        "since=2026-04-01\ndeeper=* \\ntarget\ndeeper=* \\nbegun\nc1"
    );
    let back = StampedCursor::parse(begun.to_state());
    assert_eq!(back, begun);
    assert!(back.is_band_begun());
}

#[test]
fn a_band_is_begun_only_while_a_widening_runs() {
    let none = StampedCursor::new(SyncState::new("c1"), account(since(4))).band_begun();
    assert!(!none.is_band_begun());
    assert_eq!(none.to_state().as_str(), "since=2026-04-01\nc1");
    let dropped = StampedCursor::new(SyncState::new("c1"), account(since(4)))
        .widening_to(account(since(1)))
        .band_begun()
        .widening_to(account(since(6)));
    assert!(
        !dropped.is_band_begun(),
        "a target dropped takes its band with it"
    );
    // A begun line with no target, as no engine writes, records nothing.
    let stray = read("since=2026-04-01\ndeeper=* \\nbegun\nc1");
    assert!(!stray.is_band_begun());
    assert_eq!(stray.cursor().as_str(), "c1");
}

/// The parse an engine before the widening record ran (`engine-sync`'s `window_cursor`, as it
/// stood), kept here to pin that rolling back past this version hands the provider its own
/// cursor and resumes from the same covered depth.
fn parse_before_widening(raw: &str) -> Option<(MailboxWindows, &str)> {
    let (floor, mut rest) = raw.strip_prefix("since=")?.split_once('\n')?;
    let mut depth = MailboxWindows::new(SyncWindow::since(floor.parse().ok()?));
    while let Some(line) = rest.strip_prefix("deeper=") {
        let (entry, after) = line.split_once('\n')?;
        let (floor, mailbox) = entry.split_once(' ')?;
        let window = if floor == "*" {
            SyncWindow::full()
        } else {
            SyncWindow::since(floor.parse::<CalendarDate>().ok()?)
        };
        let mailbox = MailboxId::try_from(unescape(mailbox)?.as_str()).ok()?;
        depth = depth.deepen(mailbox, window);
        rest = after;
    }
    Some((depth, rest))
}

#[test]
fn an_engine_before_the_widening_record_reads_the_same_depth_and_cursor() {
    let covered = account(since(4)).deepen(mailbox("sent"), years_back(2020));
    let target = account(years_back(2024)).deepen(mailbox("archive"), SyncWindow::full());
    let cursor = "https://graph.example.com/delta?$deltatoken=x";
    let stamped = StampedCursor::new(SyncState::new(cursor), covered.clone())
        .widening_to(target)
        .band_begun()
        .to_state();

    let (depth, provider) = parse_before_widening(stamped.as_str()).expect("it parses");
    assert_eq!(
        provider, cursor,
        "the provider would be handed something else"
    );
    assert_eq!(depth.account(), covered.account());
    assert_eq!(depth.window_for(&mailbox("sent")), years_back(2020));
    // What it reads besides are mailboxes no pass asks for, so no depth it asks is affected.
    let real: Vec<&MailboxId> = depth
        .deepened()
        .map(|(mailbox, _)| mailbox)
        .filter(|mailbox| !mailbox.as_str().starts_with('\n'))
        .collect();
    assert_eq!(real, [&mailbox("sent")]);
}

#[test]
fn a_mailbox_id_with_a_line_break_or_backslash_survives_the_record() {
    let odd = mailbox("a\\b\nc d");
    let depth = account(since(4)).deepen(odd.clone(), years_back(2020));
    let target = account(since(4)).deepen(odd, years_back(2019));
    let stamped = StampedCursor::new(SyncState::new("c1"), depth).widening_to(target);
    assert_eq!(StampedCursor::parse(stamped.to_state()), stamped);
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
        "since=2026-04-01\ndeeper=* bad\\",
        "since=2026-04-01\ndeeper=soon \\ntarget\nc1",
        "since=2026-04-01\ndeeper=* \\ntarget sent\nc1",
        "since=2026-04-01\ndeeper=* \\ntarget\ndeeper=* \\ntargetsent\nc1",
    ] {
        let stamped = read(raw);
        assert_eq!(stamped.cursor().as_str(), raw);
        assert_eq!(stamped.covered(), &MailboxWindows::default());
        assert_eq!(stamped.target(), None);
    }
}
