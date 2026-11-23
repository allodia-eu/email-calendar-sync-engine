//! The decision table, row by row. The record's own codec is `engine-core`'s
//! (`StampedCursor`), tested there.

use engine_core::{ids::MailboxId, sync::SyncWindow, time::CalendarDate};

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

fn c1() -> SyncState {
    SyncState::new("c1")
}

fn plan(stored: &SyncState, asked: &MailboxWindows) -> Plan {
    Plan::new(Some(stored.clone()), asked)
}

fn widen(from: &MailboxWindows, to: &MailboxWindows) -> Plan {
    Plan::Widen {
        cursor: c1(),
        widening: Widening::new(from.clone(), to.clone()).unwrap(),
    }
}

#[test]
fn no_cursor_is_a_fresh_pass() {
    assert_eq!(Plan::new(None, &account(since(4))), Plan::Fresh);
    assert_eq!(
        Plan::new(None, &account(since(4))).into_parts(),
        (None, None)
    );
}

#[test]
fn a_covered_depth_resumes_with_a_delta_and_so_does_a_floor_that_moved_on() {
    let stored = stamp(c1(), &account(since(4)), None, Phase::First);
    assert_eq!(plan(&stored, &account(since(4))), Plan::Delta(c1()));
    // The floor a host computes from today moves forward every day: never a widening.
    assert_eq!(plan(&stored, &account(since(5))), Plan::Delta(c1()));
    // A bare cursor covers the whole history, as every cursor written before depths did.
    for asked in [account(SyncWindow::full()), account(since(1))] {
        assert_eq!(plan(&c1(), &asked), Plan::Delta(c1()));
    }
    assert_eq!(
        plan(&stored, &account(since(4))).into_parts(),
        (Some(c1()), None)
    );
}

#[test]
fn reaching_further_back_widens_from_the_covered_depth() {
    let covered = account(since(4));
    let stored = stamp(c1(), &covered, None, Phase::First);
    for asked in [account(since(1)), account(SyncWindow::full())] {
        assert_eq!(plan(&stored, &asked), widen(&covered, &asked));
    }
    // Deepening one mailbox widens it alone.
    let sent = account(since(4)).deepen(mailbox("sent"), years_back(2020));
    assert_eq!(plan(&stored, &sent), widen(&covered, &sent));
    let Plan::Widen { widening, .. } = plan(&stored, &sent) else {
        unreachable!()
    };
    assert_eq!(
        widening.bands().len(),
        1,
        "only Sent's older mail is fetched"
    );
}

fn resume(from: &MailboxWindows, to: &MailboxWindows) -> Plan {
    Plan::Widen {
        cursor: c1(),
        widening: Widening::new(from.clone(), to.clone()).unwrap().resuming(),
    }
}

#[test]
fn a_widening_whose_band_has_begun_resumes_toward_the_depth_asked() {
    let covered = account(since(4));
    let asked = account(since(1));
    let running = Widening::new(covered.clone(), asked.clone()).unwrap();
    let begun = stamp(c1(), &asked, Some(&running), Phase::Band);
    let record = StampedCursor::parse(begun.clone());
    assert_eq!(record.covered(), &covered, "the band has not ended");
    assert_eq!(record.target(), Some(&asked));
    assert!(record.is_band_begun());
    assert_eq!(plan(&begun, &asked), resume(&covered, &asked));
    // A floor computed from today moves on daily: the band it is reaching for only shrinks.
    let a_day_on = account(SyncWindow::since(CalendarDate::new(2026, 1, 2).unwrap()));
    assert_eq!(plan(&begun, &a_day_on), resume(&covered, &a_day_on));
}

#[test]
fn a_widening_whose_band_never_began_starts_afresh() {
    let covered = account(since(4));
    let asked = account(since(1));
    let running = Widening::new(covered.clone(), asked.clone()).unwrap();
    // Only its delta committed: the cursor is the delta's, with nothing of the band in it.
    let delta_only = stamp(c1(), &asked, Some(&running), Phase::First);
    assert!(!StampedCursor::parse(delta_only.clone()).is_band_begun());
    assert_eq!(plan(&delta_only, &asked), widen(&covered, &asked));
    // A resumed widening's own delta keeps the band begun.
    let resumed = running.clone().resuming();
    let again = stamp(c1(), &asked, Some(&resumed), Phase::First);
    assert!(StampedCursor::parse(again).is_band_begun());
}

#[test]
fn a_target_changed_mid_band_restarts_from_the_covered_depth() {
    let covered = account(since(4));
    let running = Widening::new(covered.clone(), account(since(1))).unwrap();
    let stored = stamp(c1(), &account(since(1)), Some(&running), Phase::Band);
    // Further back than the target: widen afresh from what is covered, not from the target.
    let further = account(SyncWindow::full());
    assert_eq!(plan(&stored, &further), widen(&covered, &further));
    // Narrower than what is covered: the target is dropped and the pass is a delta.
    assert_eq!(plan(&stored, &account(since(6))), Plan::Delta(c1()));
    // Back to the target after that delta: afresh, for the delta may have dropped band mail.
    let after_delta = stamp(c1(), &account(since(6)), None, Phase::First);
    assert_eq!(
        plan(&after_delta, &account(since(1))),
        widen(&account(since(6)), &account(since(1)))
    );
}

#[test]
fn a_delta_stamps_the_depth_asked_and_drops_a_target() {
    let stored = stamp(c1(), &account(since(6)), None, Phase::First);
    let record = StampedCursor::parse(stored);
    assert_eq!(record.covered(), &account(since(6)));
    assert_eq!(record.target(), None);
    assert_eq!(provider_cursor(record.to_state()), c1());
}
