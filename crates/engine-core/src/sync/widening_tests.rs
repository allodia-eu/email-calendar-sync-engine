//! The band rule, checked as properties over every combination of a small universe rather than a
//! random sample of a large one.
//!
//! A window's admission is constant between floors, so three floors, the days either side of
//! each, two mailboxes that can be deepened and a third that cannot, and every mailbox set a
//! message can be filed in reach every case the rule distinguishes: an account floor moving back
//! or not, a mailbox deepened in `from`, in `to`, in both or neither, to a floor above, on or
//! below the account's, or over its whole history.

use super::*;
use crate::sync::SyncWindow;

fn day(n: u8) -> CalendarDate {
    CalendarDate::new(2026, 1, n).unwrap()
}

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

/// The floors a window or an override can take: `None` is the full history.
const FLOORS: [Option<u8>; 4] = [None, Some(10), Some(20), Some(28)];

/// The days a message can be dated: either side of every floor.
const DATES: [u8; 7] = [9, 10, 19, 20, 27, 28, 31];

fn window(floor: Option<u8>) -> SyncWindow {
    floor.map_or_else(SyncWindow::full, |n| SyncWindow::since(day(n)))
}

/// Every window over the universe: each account floor, with each of `a` and `b` left alone or
/// held to any floor.
fn windows() -> Vec<MailboxWindows> {
    let overrides: Vec<Option<Option<u8>>> = core::iter::once(None)
        .chain(FLOORS.iter().copied().map(Some))
        .collect();
    let mut all = Vec::new();
    for account in FLOORS {
        for a in &overrides {
            for b in &overrides {
                let mut windows = MailboxWindows::new(window(account));
                if let Some(floor) = a {
                    windows = windows.deepen(mailbox("a"), window(*floor));
                }
                if let Some(floor) = b {
                    windows = windows.deepen(mailbox("b"), window(*floor));
                }
                all.push(windows);
            }
        }
    }
    all
}

/// Every message over the universe: filed in any subset of `a`, `b` and `c`, on every date.
fn messages() -> Vec<(Vec<MailboxId>, Option<UtcDateTime>)> {
    let names = ["a", "b", "c"];
    let mut all = Vec::new();
    for set in 0u8..8 {
        let filed: Vec<MailboxId> = names
            .iter()
            .enumerate()
            .filter(|(bit, _)| set & (1 << bit) != 0)
            .map(|(_, name)| mailbox(name))
            .collect();
        for date in DATES {
            let at = UtcDateTime::new(2026, 1, date, 13, 0, 0).unwrap();
            all.push((filed.clone(), Some(at)));
        }
        all.push((filed, None));
    }
    all
}

#[test]
fn a_widening_exists_exactly_when_the_wider_window_admits_something_new() {
    let messages = messages();
    for from in windows() {
        for to in windows() {
            let gains = messages
                .iter()
                .any(|(filed, date)| to.admits(filed, *date) && !from.admits(filed, *date));
            assert_eq!(
                Widening::new(from.clone(), to.clone()).is_some(),
                gains,
                "{from:?} to {to:?}"
            );
        }
    }
}

#[test]
fn the_bands_leave_no_gap_and_never_reach_past_the_wider_window() {
    let messages = messages();
    for from in windows() {
        for to in windows() {
            let Some(widening) = Widening::new(from.clone(), to.clone()) else {
                continue;
            };
            let bands = widening.bands();
            assert!(!bands.is_empty(), "{from:?} to {to:?} has no band");
            for (filed, date) in &messages {
                let newly = to.admits(filed, *date) && !from.admits(filed, *date);
                let listed = bands
                    .iter()
                    .filter(|band| band.admits(filed, *date))
                    .count();
                let case = || format!("{from:?} to {to:?}: {filed:?} on {date:?}");
                assert!(!newly || listed > 0, "a gap: {}", case());
                assert!(
                    listed == 0 || to.admits(filed, *date),
                    "past the window: {}",
                    case()
                );
                // Mail filed in one mailbox is listed at most once, and only when it is new, but
                // for one case: what `from` held further back in that mailbox, which the account
                // band lists again once the account's floor moves past it. Mail filed in two
                // deepened mailboxes may be listed by both bands.
                if let [only] = filed.as_slice() {
                    assert!(listed <= 1, "listed twice: {}", case());
                    let held_deeper = from.window_for(only) != from.account();
                    assert!(
                        newly || listed == 0 || held_deeper,
                        "listed again: {}",
                        case()
                    );
                } else if filed.is_empty() {
                    assert_eq!(listed, usize::from(newly), "{}", case());
                }
            }
        }
    }
}

#[test]
fn the_account_band_and_a_mailbox_band_never_share_a_day() {
    let anywhere = [mailbox("a"), mailbox("b")];
    for from in windows() {
        for to in windows() {
            let Some(widening) = Widening::new(from.clone(), to.clone()) else {
                continue;
            };
            let bands = widening.bands();
            let account: Vec<&Band> = bands.iter().filter(|b| b.mailbox().is_none()).collect();
            assert!(account.len() <= 1, "{from:?} to {to:?}");
            for band in bands.iter().filter(|b| b.mailbox().is_some()) {
                for account in &account {
                    for date in DATES {
                        let at = Some(UtcDateTime::new(2026, 1, date, 13, 0, 0).unwrap());
                        assert!(
                            !(band.admits(&anywhere, at) && account.admits(&anywhere, at)),
                            "{from:?} to {to:?}: {band:?} and {account:?} share {date}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_folder_band_is_exactly_what_its_folder_gains() {
    let messages = messages();
    for from in windows() {
        for to in windows() {
            let Some(widening) = Widening::new(from.clone(), to.clone()) else {
                continue;
            };
            for folder in [mailbox("a"), mailbox("b"), mailbox("c")] {
                let band = widening.band_for(&folder);
                for (_, date) in messages.iter().filter(|(_, date)| date.is_some()) {
                    let filed = core::slice::from_ref(&folder);
                    let gains = to.admits(filed, *date) && !from.admits(filed, *date);
                    assert_eq!(
                        band.as_ref().is_some_and(|band| band.admits(filed, *date)),
                        gains,
                        "{from:?} to {to:?} in {folder:?} on {date:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_window_lists_as_its_bands_from_nothing_held() {
    for to in windows() {
        let bands = to.bands();
        for (filed, date) in messages().iter().filter(|(_, date)| date.is_some()) {
            let listed = bands
                .iter()
                .filter(|band| band.admits(filed, *date))
                .count();
            assert_eq!(
                listed > 0,
                to.admits(filed, *date),
                "{to:?}: {filed:?} {date:?}"
            );
            if filed.len() <= 1 {
                assert!(listed <= 1, "{to:?}: {filed:?} on {date:?} listed twice");
            }
        }
    }
}

#[test]
fn the_bands_have_the_shape_the_rule_names() {
    let windows = MailboxWindows::new(SyncWindow::since(day(20)))
        .deepen(mailbox("sent"), SyncWindow::since(day(10)))
        .deepen(mailbox("archive"), SyncWindow::full());
    let wider = MailboxWindows::new(SyncWindow::since(day(10)))
        .deepen(mailbox("sent"), SyncWindow::since(day(5)))
        .deepen(mailbox("archive"), SyncWindow::full());
    let widening = Widening::new(windows.clone(), wider.clone()).unwrap();
    assert_eq!(widening.from(), &windows);
    assert_eq!(widening.to(), &wider);
    let bands = widening.bands();
    let shape: Vec<_> = bands
        .iter()
        .map(|band| {
            (
                band.mailbox().map(MailboxId::as_str),
                band.since(),
                band.before(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (None, Some(day(10)), Some(day(20))),
            (Some("sent"), Some(day(5)), Some(day(10))),
        ],
        "archive was held whole, and Sent's band stops at its old floor"
    );
    let listed = windows.bands();
    let listing: Vec<_> = listed
        .iter()
        .map(|band| {
            (
                band.mailbox().map(MailboxId::as_str),
                band.since(),
                band.before(),
            )
        })
        .collect();
    assert_eq!(
        listing,
        [
            (None, Some(day(20)), None),
            (Some("archive"), None, Some(day(20))),
            (Some("sent"), Some(day(10)), Some(day(20))),
        ]
    );
    assert_eq!(
        MailboxWindows::default().bands(),
        [Band {
            mailbox: None,
            since: None,
            before: None
        }],
        "the full history is one listing of everything"
    );
    assert_eq!(
        Widening::new(windows.clone(), windows),
        None,
        "nothing to widen"
    );
}
