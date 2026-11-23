//! The sync depth window, for an account and for the mailboxes held further back than it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    ids::MailboxId,
    time::{CalendarDate, UtcDateTime},
};

/// How far back a mail sync fetches — the "sync depth" a host bounds a first
/// snapshot/backfill to, so a large mailbox syncs only recent mail.
///
/// It is a **per-sync argument**, not a provider-construction detail: a host
/// changes depth by passing a different window to the next sync, without
/// reconnecting providers. Provider-neutral — each adapter maps `since` to its
/// protocol's date filter: IMAP `UID SEARCH SINCE <date>`, JMAP an `Email/query`
/// `after` filter on `receivedAt`, Microsoft Graph a `receivedDateTime ge` filter.
///
/// It bounds what a pass **fetches**, and — through [`admits`](Self::admits) — what a **delta**
/// is allowed to store. The two are not the same job: a delta's "new arrivals" are new to *us*,
/// not necessarily recent. IMAP has no in-place edit, so filing a three-year-old message into a
/// folder mints a UID above the cursor and the delta carries it as an arrival; Graph, Gmail and
/// JMAP deltas report a moved message the same way. Left unfiltered, mail the user asked this
/// device not to keep walks back in through every protocol, and no later pass removes it —
/// a delta never re-lists what it did not change.
///
/// A **snapshot** is not filtered against this: its present set is the adapter's own enumeration
/// of the window, in its server's date semantics, and the store tombstones by diffing against
/// it (`store-and-sync.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncWindow {
    /// The oldest date to fetch, inclusive, or `None` for the full history.
    pub since: Option<CalendarDate>,
}

impl SyncWindow {
    /// A window with no floor — the entire mailbox history.
    #[must_use]
    pub fn full() -> Self {
        Self { since: None }
    }

    /// A window bounded to mail delivered on or after `since`.
    #[must_use]
    pub fn since(since: CalendarDate) -> Self {
        Self { since: Some(since) }
    }

    /// Mail on or after the day `months` whole calendar months before `today`
    /// ([`CalendarDate::checked_months_before`]: the day clamped to the month it lands in). A
    /// floor below the representable range is the full history; `months == 0` is a floor of
    /// `today` itself.
    ///
    /// The engine reads no clock here: `today` is the host's day, so a window built from it means
    /// the same thing wherever it is built, and a test names its day. "All mail" is
    /// [`full`](Self::full), never a sentinel month count.
    #[must_use]
    pub fn months_back(months: u32, today: CalendarDate) -> Self {
        today
            .checked_months_before(months)
            .map_or_else(Self::full, Self::since)
    }

    /// Whether everything `other` admits, `self` admits too: `self` reaches at least as far back.
    ///
    /// The full history covers every window, and is covered by none but itself.
    #[must_use]
    pub fn covers(self, other: Self) -> bool {
        match (self.since, other.since) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(ours), Some(theirs)) => ours <= theirs,
        }
    }

    /// The date floor, if any.
    #[must_use]
    pub fn floor(&self) -> Option<CalendarDate> {
        self.since
    }

    /// Whether this window bounds the sync at all (`false` for the full history).
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.since.is_some()
    }

    /// Whether a message dated `date` belongs inside this window.
    ///
    /// `date` is the message's `received_at` falling back to `sent_at` — the same value
    /// `MailRow::date_utc` carries and every adapter maps its protocol's date filter to, so
    /// what a pass fetches and what the engine keeps cannot disagree.
    ///
    /// Two rules, both deliberate:
    ///
    /// - The floor is **inclusive** and compared as a **date**, not an instant. A message sent at
    ///   any hour of the floor day is inside. A provider's date filter has date granularity (IMAP
    ///   `SINCE` takes `dd-Mon-yyyy`), so an instant comparison here would reject mail the fetch
    ///   deliberately asked for.
    /// - **Undated mail is admitted.** A message with neither timestamp is not provably outside the
    ///   window, and dropping it would lose mail on the strength of a missing header.
    ///
    /// An unbounded window admits everything.
    #[must_use]
    pub fn admits(&self, date: Option<UtcDateTime>) -> bool {
        let (Some(floor), Some(date)) = (self.since, date) else {
            return true;
        };
        CalendarDate::new(date.year(), date.month(), date.day()).is_ok_and(|date| date >= floor)
    }

    /// Whichever of `self` and `other` reaches further back.
    fn wider(self, other: Self) -> Self {
        match (self.since, other.since) {
            (Some(ours), Some(theirs)) => Self::since(ours.min(theirs)),
            _ => Self::full(),
        }
    }
}

/// One account's sync window, with some of its mailboxes held further back than the rest.
///
/// A host that wants one mailbox's longer history (its Sent mail, say) deepens that mailbox here
/// rather than widening the whole account. A deepened mailbox is never held to *less* than the
/// account's window: its window is the wider of the two, so an override can only reach further
/// back.
///
/// A message filed in several mailboxes (a JMAP or Gmail message) belongs inside when the window
/// of **any** of them admits it. On a provider whose mail scope is one folder (IMAP, Graph) a
/// message is filed in exactly that folder, so the rule reduces to the folder's own window.
///
/// What a pass fetches, what a delta may store and what a local prune keeps all read the depth
/// from here, so the three cannot disagree about a mailbox (`store-and-sync.md`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailboxWindows {
    account: SyncWindow,
    deeper: BTreeMap<MailboxId, SyncWindow>,
}

impl MailboxWindows {
    /// The account's window, with no mailbox held further back.
    #[must_use]
    pub fn new(account: SyncWindow) -> Self {
        Self {
            account,
            deeper: BTreeMap::new(),
        }
    }

    /// Holds `mailbox` to `window` wherever that reaches further back than the account's
    /// (builder-style). A second call for the same mailbox replaces the first.
    #[must_use]
    pub fn deepen(mut self, mailbox: MailboxId, window: SyncWindow) -> Self {
        self.deeper.insert(mailbox, window);
        self
    }

    /// The account's own window: the one every mailbox that was not deepened syncs under.
    #[must_use]
    pub fn account(&self) -> SyncWindow {
        self.account
    }

    /// The window `mailbox` syncs under: the wider of the account's and its own.
    #[must_use]
    pub fn window_for(&self, mailbox: &MailboxId) -> SyncWindow {
        self.deeper
            .get(mailbox)
            .map_or(self.account, |own| self.account.wider(*own))
    }

    /// The mailboxes that reach further back than the account, each with the window it syncs
    /// under, in mailbox order. An override that reaches no further than the account's window is
    /// left out, so every one is when the account's window is the full history.
    ///
    /// This is what an adapter whose mail scope is the whole account fetches beyond the
    /// account's window.
    pub fn deepened(&self) -> impl Iterator<Item = (&MailboxId, SyncWindow)> + '_ {
        self.deeper.iter().filter_map(|(mailbox, own)| {
            let window = self.account.wider(*own);
            (window != self.account).then_some((mailbox, window))
        })
    }

    /// Whether everything `other` admits, `self` admits too, for a message filed in any mailboxes
    /// at all: the account's window covers `other`'s, and so does the window of every mailbox
    /// `other` holds further back.
    ///
    /// This is the question a stored cursor's depth answers before a pass resumes from it, and
    /// the one [`Widening::new`](super::Widening::new) asks before there is anything to widen.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.account.covers(other.account)
            && other
                .deepened()
                .all(|(mailbox, window)| self.window_for(mailbox).covers(window))
    }

    /// Whether a message filed in `mailboxes` and dated `date` belongs inside: the window of any
    /// mailbox it is filed in admits it, by the rules of [`SyncWindow::admits`]. Undated mail is
    /// admitted, as it is there.
    #[must_use]
    pub fn admits<'a>(
        &self,
        mailboxes: impl IntoIterator<Item = &'a MailboxId>,
        date: Option<UtcDateTime>,
    ) -> bool {
        self.account.admits(date)
            || mailboxes
                .into_iter()
                .any(|mailbox| self.deeper.get(mailbox).is_some_and(|own| own.admits(date)))
    }
}

impl From<SyncWindow> for MailboxWindows {
    fn from(account: SyncWindow) -> Self {
        Self::new(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u8, day: u8) -> CalendarDate {
        CalendarDate::new(year, month, day).unwrap()
    }

    fn mailbox(id: &str) -> MailboxId {
        MailboxId::try_from(id).unwrap()
    }

    #[test]
    fn a_deepened_mailbox_reaches_back_and_every_other_keeps_the_account_window() {
        let account = SyncWindow::since(date(2026, 4, 1));
        let windows = MailboxWindows::new(account)
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)));

        assert_eq!(windows.account(), account);
        assert_eq!(
            windows.window_for(&mailbox("sent")),
            SyncWindow::since(date(2024, 4, 1))
        );
        assert_eq!(windows.window_for(&mailbox("inbox")), account);
    }

    #[test]
    fn an_override_never_narrows_a_mailbox() {
        let account = SyncWindow::since(date(2026, 4, 1));
        let narrower = MailboxWindows::new(account)
            .deepen(mailbox("sent"), SyncWindow::since(date(2026, 6, 1)));
        assert_eq!(narrower.window_for(&mailbox("sent")), account);

        // An unbounded account window is already the widest there is.
        let full = MailboxWindows::new(SyncWindow::full())
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)));
        assert_eq!(full.window_for(&mailbox("sent")), SyncWindow::full());

        // And an unbounded override holds that mailbox over its whole history.
        let everything = MailboxWindows::new(account).deepen(mailbox("sent"), SyncWindow::full());
        assert_eq!(everything.window_for(&mailbox("sent")), SyncWindow::full());
    }

    #[test]
    fn only_a_mailbox_reaching_past_the_account_is_deepened() {
        let account = SyncWindow::since(date(2026, 4, 1));
        let windows = MailboxWindows::new(account)
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)))
            .deepen(mailbox("archive"), SyncWindow::full())
            .deepen(mailbox("inbox"), SyncWindow::since(date(2026, 6, 1)))
            .deepen(mailbox("drafts"), account);
        let deepened: Vec<(&str, SyncWindow)> = windows
            .deepened()
            .map(|(mailbox, window)| (mailbox.as_str(), window))
            .collect();
        assert_eq!(
            deepened,
            [
                ("archive", SyncWindow::full()),
                ("sent", SyncWindow::since(date(2024, 4, 1))),
            ]
        );

        let unbounded = MailboxWindows::new(SyncWindow::full())
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)));
        assert_eq!(unbounded.deepened().count(), 0);
    }

    #[test]
    fn a_message_is_admitted_when_any_mailbox_it_is_filed_in_admits_it() {
        let windows = MailboxWindows::new(SyncWindow::since(date(2026, 4, 1)))
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)));
        let old = Some("2025-01-10T09:00:00Z".parse::<UtcDateTime>().unwrap());
        let older = Some("2023-01-10T09:00:00Z".parse::<UtcDateTime>().unwrap());

        assert!(windows.admits(&[mailbox("sent")], old));
        assert!(
            windows.admits(&[mailbox("inbox"), mailbox("sent")], old),
            "filed in the deepened mailbox as well as another"
        );
        assert!(!windows.admits(&[mailbox("inbox")], old));
        assert!(
            !windows.admits(&[mailbox("sent")], older),
            "past the override too"
        );
        assert!(windows.admits(&[mailbox("inbox")], None), "undated is kept");
        assert!(
            windows.admits(
                core::iter::empty(),
                Some("2026-05-01T00:00:00Z".parse().unwrap())
            ),
            "the account window alone admits in-window mail"
        );
    }

    #[test]
    fn a_plain_window_converts_with_no_mailbox_deepened() {
        let account = SyncWindow::since(date(2026, 4, 1));
        assert_eq!(MailboxWindows::from(account), MailboxWindows::new(account));
        assert_eq!(MailboxWindows::default().account(), SyncWindow::full());
    }

    #[test]
    fn months_back_counts_whole_months_from_today_and_saturates_to_the_full_history() {
        let today = date(2026, 3, 31);
        assert_eq!(
            SyncWindow::months_back(1, today),
            SyncWindow::since(date(2026, 2, 28))
        );
        assert_eq!(
            SyncWindow::months_back(24, today),
            SyncWindow::since(date(2024, 3, 31))
        );
        assert_eq!(SyncWindow::months_back(0, today), SyncWindow::since(today));
        assert_eq!(SyncWindow::months_back(u32::MAX, today), SyncWindow::full());
    }

    #[test]
    fn a_window_covers_the_windows_it_reaches_at_least_as_far_back_as() {
        let april = SyncWindow::since(date(2026, 4, 1));
        let march = SyncWindow::since(date(2026, 3, 1));
        assert!(march.covers(april));
        assert!(april.covers(april));
        assert!(!april.covers(march));
        assert!(SyncWindow::full().covers(march));
        assert!(SyncWindow::full().covers(SyncWindow::full()));
        assert!(!march.covers(SyncWindow::full()));
    }

    #[test]
    fn mailbox_windows_cover_by_the_account_and_by_every_mailbox_held_further_back() {
        let account = MailboxWindows::new(SyncWindow::since(date(2026, 4, 1)));
        let sent = account
            .clone()
            .deepen(mailbox("sent"), SyncWindow::since(date(2024, 4, 1)));
        assert!(sent.covers(&account));
        assert!(!account.covers(&sent), "Sent's older mail is not held");
        assert!(sent.covers(&sent));
        let later = MailboxWindows::new(SyncWindow::since(date(2026, 5, 1)))
            .deepen(mailbox("sent"), SyncWindow::since(date(2025, 1, 1)));
        assert!(sent.covers(&later));
        assert!(!later.covers(&sent));
        let wider_account = MailboxWindows::new(SyncWindow::since(date(2026, 3, 1)));
        assert!(!sent.covers(&wider_account), "the account reaches past it");
        assert!(MailboxWindows::default().covers(&sent));
        // An override reaching no further than the account's window asks for nothing more.
        let shallow = account
            .clone()
            .deepen(mailbox("inbox"), SyncWindow::since(date(2026, 6, 1)));
        assert!(account.covers(&shallow));
    }

    #[test]
    fn full_window_has_no_floor() {
        let window = SyncWindow::full();
        assert_eq!(window, SyncWindow::default());
        assert!(!window.is_bounded());
        assert_eq!(window.floor(), None);
    }

    #[test]
    fn bounded_window_carries_its_floor() {
        let floor = CalendarDate::new(2026, 1, 7).unwrap();
        let window = SyncWindow::since(floor);
        assert!(window.is_bounded());
        assert_eq!(window.floor(), Some(floor));
    }

    #[test]
    fn roundtrips_through_json() {
        let window = SyncWindow::since(CalendarDate::new(2026, 1, 7).unwrap());
        let json = serde_json::to_string(&window).unwrap();
        assert_eq!(json, r#"{"since":"2026-01-07"}"#);
        assert_eq!(serde_json::from_str::<SyncWindow>(&json).unwrap(), window);
    }

    #[test]
    fn admits_mail_on_or_after_an_inclusive_floor() {
        let window = SyncWindow::since(CalendarDate::new(2026, 4, 1).unwrap());
        let at = |text: &str| Some(text.parse::<UtcDateTime>().unwrap());

        assert!(!window.admits(at("2026-03-31T23:59:59Z")), "day before");
        // The floor is a date, so any hour of it is inside — a provider's own filter has
        // date granularity, and an instant comparison would drop mail the fetch asked for.
        assert!(window.admits(at("2026-04-01T00:00:00Z")), "floor midnight");
        assert!(
            window.admits(at("2026-04-01T23:59:59Z")),
            "floor end of day"
        );
        assert!(window.admits(at("2026-06-20T09:00:00Z")), "well inside");
    }

    #[test]
    fn undated_mail_and_an_unbounded_window_admit_everything() {
        let bounded = SyncWindow::since(CalendarDate::new(2026, 4, 1).unwrap());
        // Neither timestamp: not provably outside, so keeping it is the only safe answer.
        assert!(bounded.admits(None));
        // No floor: nothing is outside it.
        let old = Some("1999-01-01T00:00:00Z".parse::<UtcDateTime>().unwrap());
        assert!(SyncWindow::full().admits(old));
        assert!(SyncWindow::full().admits(None));
    }
}
