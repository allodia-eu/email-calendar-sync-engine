//! What a Gmail backfill or reconcile enumerates for a sync window, over the API and over IMAP
//! alike.
//!
//! Gmail syncs the whole account's mail as one scope, so one enumeration serves every label. The
//! account's listing holds every message on or after the account's floor. A label held further
//! back (`MailboxWindows::deepened`) adds a listing of that label alone, from its own floor to
//! the account's, so no other label's older mail is fetched. A label the account does not have
//! lists nothing (observed: `messages.list` answers an unknown `labelIds` with no messages).
//!
//! The two bounds meet at the account's floor and both include it (observed: `after:` and
//! `before:` each list a message dated exactly on the epoch they name), so a message dated on
//! that instant is listed twice rather than missed; the store writes it once.

use engine_core::{sync::MailboxWindows, time::CalendarDate};

/// One enumeration of a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    /// The label listed alone, or `None` for every message.
    pub(crate) label: Option<String>,
    /// The earliest day listed, or `None` for the whole history.
    pub(crate) after: Option<CalendarDate>,
    /// The day the listing stops at, or `None` for no end.
    pub(crate) before: Option<CalendarDate>,
}

/// The listings `window` enumerates: the account's first, then each label held further back,
/// in label order.
pub(crate) fn listings(window: &MailboxWindows) -> Vec<Listing> {
    let account = window.account().floor();
    let mut listings = vec![Listing {
        label: None,
        after: account,
        before: None,
    }];
    listings.extend(window.deepened().map(|(label, deeper)| Listing {
        label: Some(label.as_str().to_owned()),
        after: deeper.floor(),
        before: account,
    }));
    listings
}

#[cfg(test)]
mod tests {
    use engine_core::{ids::MailboxId, sync::SyncWindow};

    use super::*;

    fn day(year: i32) -> CalendarDate {
        CalendarDate::new(year, 4, 1).unwrap()
    }

    #[test]
    fn the_account_is_listed_first_then_each_deepened_label_back_to_its_floor() {
        let window = MailboxWindows::new(SyncWindow::since(day(2026)))
            .deepen(
                MailboxId::try_from("SENT").unwrap(),
                SyncWindow::since(day(2020)),
            )
            .deepen(MailboxId::try_from("Label_1").unwrap(), SyncWindow::full())
            .deepen(
                MailboxId::try_from("INBOX").unwrap(),
                SyncWindow::since(day(2027)),
            );
        assert_eq!(
            listings(&window),
            [
                Listing {
                    label: None,
                    after: Some(day(2026)),
                    before: None,
                },
                Listing {
                    label: Some("Label_1".to_owned()),
                    after: None,
                    before: Some(day(2026)),
                },
                Listing {
                    label: Some("SENT".to_owned()),
                    after: Some(day(2020)),
                    before: Some(day(2026)),
                },
            ]
        );
    }

    #[test]
    fn the_full_history_is_one_listing() {
        let window = MailboxWindows::new(SyncWindow::full()).deepen(
            MailboxId::try_from("SENT").unwrap(),
            SyncWindow::since(day(2020)),
        );
        assert_eq!(
            listings(&window),
            [Listing {
                label: None,
                after: None,
                before: None,
            }]
        );
    }
}
