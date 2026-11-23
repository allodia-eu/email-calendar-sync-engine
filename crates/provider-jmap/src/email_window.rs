//! The `Email/query` filter a pass fetches under (RFC 8621 §4.4.1).
//!
//! JMAP syncs the whole account's mail as one `Email` scope, so one query serves every mailbox.
//! What it lists is a set of bands ([`Band`]): the account from its floor, and each mailbox held
//! further back from its own floor up to the account's, for a fresh pass
//! ([`MailboxWindows::bands`]); only what a wider window adds, for a widening
//! ([`Widening::bands`](engine_core::sync::Widening::bands)). Each band is one condition,
//! `{inMailbox, after, before}`, and the conditions are joined by an `OR` (RFC 8620 §5.5
//! `FilterOperator`). The rule that computes the bands is `engine-core`'s, so this module only
//! says how JMAP writes one.
//!
//! `after` is "same as or after" and `before` "strictly before" (RFC 8621 §4.4.1), both on
//! `receivedAt`, a UTC instant: a band ending `before` a day and a window starting `after` it are
//! exact complements, and they cut on the same UTC day the engine judges mail by.
//!
//! A delta carries no filter (`Email/changes` takes none), so an old message moved into scope
//! is reported as a change; the engine judges it on apply (`MailboxWindows::admits`), which is
//! where the delta's bound holds for every adapter. The snapshot's bound is this filter, and is
//! not re-checked there.

use engine_core::{
    sync::{Band, MailboxWindows},
    time::CalendarDate,
};
use serde_json::{Map, Value, json};

/// The filter for `window`, or `None` when it is the full history.
pub(crate) fn email_filter(window: &MailboxWindows) -> Option<Value> {
    band_filter(&window.bands())
}

/// The filter listing `bands`, or `None` when one of them is everything (no filter at all).
pub(crate) fn band_filter(bands: &[Band]) -> Option<Value> {
    let mut conditions = Vec::with_capacity(bands.len());
    for band in bands {
        let mut condition = Map::new();
        if let Some(mailbox) = band.mailbox() {
            condition.insert("inMailbox".to_owned(), json!(mailbox.as_str()));
        }
        if let Some(since) = band.since() {
            condition.insert("after".to_owned(), json!(instant(since)));
        }
        if let Some(before) = band.before() {
            condition.insert("before".to_owned(), json!(instant(before)));
        }
        if condition.is_empty() {
            return None;
        }
        conditions.push(Value::Object(condition));
    }
    match conditions.len() {
        0 => None,
        1 => conditions.pop(),
        _ => Some(json!({ "operator": "OR", "conditions": conditions })),
    }
}

/// A day as `receivedAt` compares it: its first instant, in UTC.
fn instant(day: CalendarDate) -> String {
    format!("{day}T00:00:00Z")
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::MailboxId,
        sync::{SyncWindow, Widening},
    };

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
                    { "inMailbox": "archive", "before": "2026-04-01T00:00:00Z" },
                    {
                        "inMailbox": "sent",
                        "after": "2020-04-01T00:00:00Z",
                        "before": "2026-04-01T00:00:00Z",
                    },
                ],
            }))
        );
    }

    #[test]
    fn a_widening_lists_only_what_the_wider_window_adds() {
        let widening = Widening::new(
            MailboxWindows::new(since(2026)),
            MailboxWindows::new(since(2025)).deepen(mailbox("sent"), since(2020)),
        )
        .unwrap();
        assert_eq!(
            band_filter(&widening.bands()),
            Some(json!({
                "operator": "OR",
                "conditions": [
                    { "after": "2025-04-01T00:00:00Z", "before": "2026-04-01T00:00:00Z" },
                    {
                        "inMailbox": "sent",
                        "after": "2020-04-01T00:00:00Z",
                        "before": "2025-04-01T00:00:00Z",
                    },
                ],
            }))
        );
        let everything =
            Widening::new(MailboxWindows::new(since(2026)), MailboxWindows::default()).unwrap();
        assert_eq!(
            band_filter(&everything.bands()),
            Some(json!({ "before": "2026-04-01T00:00:00Z" }))
        );
        assert_eq!(band_filter(&[]), None);
    }
}
