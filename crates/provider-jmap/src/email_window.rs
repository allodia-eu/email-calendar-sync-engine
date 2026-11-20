//! The `Email/query` filter a snapshot fetches under (RFC 8621 §4.4.1).
//!
//! JMAP syncs the whole account's mail as one `Email` scope, so one query serves every mailbox.
//! A mailbox held further back than the account adds an `inMailbox` condition with its own
//! floor, joined to the account's floor by an `OR` (RFC 8620 §5.5 `FilterOperator`), so the
//! snapshot fetches that mailbox's older mail and no other mailbox's.
//!
//! A delta carries no filter (`Email/changes` takes none), so an old message moved into scope
//! is reported as a change; the engine judges it on apply (`MailboxWindows::admits`), which is
//! where the delta's bound holds for every adapter. The snapshot's bound is this filter, and is
//! not re-checked there.

use engine_core::{sync::MailboxWindows, time::CalendarDate};
use serde_json::{Value, json};

/// The filter for `window`, or `None` when the account's window is the full history.
pub(crate) fn email_filter(window: &MailboxWindows) -> Option<Value> {
    let account = json!({ "after": after(window.account().floor()?) });
    let mut conditions = vec![account.clone()];
    for (mailbox, deeper) in window.deepened() {
        conditions.push(match deeper.floor() {
            Some(floor) => json!({ "inMailbox": mailbox.as_str(), "after": after(floor) }),
            None => json!({ "inMailbox": mailbox.as_str() }),
        });
    }
    Some(if conditions.len() == 1 {
        account
    } else {
        json!({ "operator": "OR", "conditions": conditions })
    })
}

/// A floor as `receivedAt` compares it: the first instant of the day, in UTC.
fn after(floor: CalendarDate) -> String {
    format!("{floor}T00:00:00Z")
}

#[cfg(test)]
mod tests {
    use engine_core::{ids::MailboxId, sync::SyncWindow};

    use super::*;

    fn since(year: i32) -> SyncWindow {
        SyncWindow::since(CalendarDate::new(year, 4, 1).unwrap())
    }

    fn mailbox(id: &str) -> MailboxId {
        MailboxId::try_from(id).unwrap()
    }

    #[test]
    fn the_full_history_takes_no_filter() {
        let window = MailboxWindows::new(SyncWindow::full()).deepen(mailbox("sent"), since(2020));
        assert_eq!(email_filter(&window), None);
    }

    #[test]
    fn the_account_floor_alone_is_one_condition() {
        assert_eq!(
            email_filter(&MailboxWindows::new(since(2026))),
            Some(json!({ "after": "2026-04-01T00:00:00Z" }))
        );
    }

    #[test]
    fn a_deepened_mailbox_adds_its_own_floor_beside_the_accounts() {
        let window = MailboxWindows::new(since(2026))
            .deepen(mailbox("sent"), since(2020))
            .deepen(mailbox("archive"), SyncWindow::full())
            .deepen(mailbox("inbox"), since(2027));
        assert_eq!(
            email_filter(&window),
            Some(json!({
                "operator": "OR",
                "conditions": [
                    { "after": "2026-04-01T00:00:00Z" },
                    { "inMailbox": "archive" },
                    { "inMailbox": "sent", "after": "2020-04-01T00:00:00Z" },
                ],
            }))
        );
    }
}
