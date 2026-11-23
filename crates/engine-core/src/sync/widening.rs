//! Going from the mail a store covers to a wider window, as listings an adapter can send.
//!
//! A host that held three months and now wants six must not read the three months again. What it
//! lacks is a set of date ranges, per mailbox or for any: the mail the wider window admits and the
//! narrower one does not. [`Widening`] names the two windows and [`Widening::bands`] computes those
//! ranges by one rule, so no adapter derives them. The same rule with no window held enumerates
//! a whole window ([`MailboxWindows::bands`]), which is what an adapter whose scope is the whole
//! account sends for a fresh pass.
//!
//! The rule (`docs/design/windows-and-widening.md`, §3.2):
//!
//! - **The account band** `[to.account, from.account)`, in any mailbox, when the account's floor
//!   moved back. With nothing held, it is `[to.account, …)`, with no upper end.
//! - **A mailbox band** `[to.window_for(m), min(from.window_for(m), to.account))` for each mailbox
//!   `m` that `to` holds further back than its account, when not empty. It stops at the new account
//!   floor because everything above that is in the account band or already held.
//!
//! The guarantee is **at least once per message, never a gap**: every message the wider window
//! admits and the narrower one does not is admitted by some band, and no band admits a message the
//! wider window does not. A repeat is the same key, which a store upserts once, and there are two
//! sources of one. Two mailbox bands may share a date, so a message filed in both is listed by
//! each. And the account band lists any mailbox, so the older mail of a mailbox `from` already held
//! further back is listed again once the account's floor moves past it. Otherwise a message
//! filed in one mailbox is listed once if it is new and not at all if it was held, and the account
//! band and a mailbox band never share a date.

use crate::{
    ids::MailboxId,
    sync::MailboxWindows,
    time::{CalendarDate, UtcDateTime},
};

/// Mail dated in `[since, before)`, filed in one mailbox or in any: one listing an adapter sends.
///
/// Both ends compare a message's date as [`SyncWindow::admits`](super::SyncWindow::admits) does, as
/// a day: `since` is inclusive (IMAP `SINCE`, JMAP `after`), `before` exclusive (IMAP `BEFORE`,
/// JMAP `before`), so a band and the window that ends where it begins are exact complements on the
/// server's own clock. A `since` of `None` reaches the full history; a `before` of `None` has no
/// upper end, which only a whole window's listing has ([`MailboxWindows::bands`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Band {
    mailbox: Option<MailboxId>,
    since: Option<CalendarDate>,
    before: Option<CalendarDate>,
}

impl Band {
    /// The one mailbox this band lists, or `None` for mail in any mailbox.
    #[must_use]
    pub fn mailbox(&self) -> Option<&MailboxId> {
        self.mailbox.as_ref()
    }

    /// The oldest day listed, inclusive, or `None` for the full history.
    #[must_use]
    pub fn since(&self) -> Option<CalendarDate> {
        self.since
    }

    /// The day the band stops before, exclusive, or `None` when it has no upper end.
    #[must_use]
    pub fn before(&self) -> Option<CalendarDate> {
        self.before
    }

    /// Whether a message filed in `mailboxes` and dated `date` is in this band.
    ///
    /// Undated mail is in no band: every window admits it
    /// ([`SyncWindow::admits`](super::SyncWindow::admits)), so no
    /// widening newly admits it, and no listing by date can name it.
    #[must_use]
    pub fn admits<'a>(
        &self,
        mailboxes: impl IntoIterator<Item = &'a MailboxId>,
        date: Option<UtcDateTime>,
    ) -> bool {
        let Some(day) = date.and_then(|at| CalendarDate::new(at.year(), at.month(), at.day()).ok())
        else {
            return false;
        };
        let in_mailbox = self
            .mailbox
            .as_ref()
            .is_none_or(|own| mailboxes.into_iter().any(|mailbox| mailbox == own));
        in_mailbox
            && self.since.is_none_or(|since| day >= since)
            && self.before.is_none_or(|before| day < before)
    }

    /// The band `[since, before)`, or `None` when it holds no day.
    fn between(
        mailbox: Option<&MailboxId>,
        since: Option<CalendarDate>,
        before: Option<CalendarDate>,
    ) -> Option<Self> {
        let empty = matches!((since, before), (Some(since), Some(before)) if since >= before);
        (!empty).then(|| Self {
            mailbox: mailbox.cloned(),
            since,
            before,
        })
    }
}

/// Going from the windows a store covers to wider ones.
///
/// [`new`](Self::new) refuses a pair with nothing to widen, so a `Widening` always has at least one
/// band. A host that keeps its own record persists `from` and `to` beside its cursor while the
/// widening runs, and records `to` as covered only once every scope's bands have ended.
///
/// **Whether a widening continues one that was interrupted is the caller's decision, never the
/// adapter's** ([`resuming`](Self::resuming)). Only the caller knows what happened to the scope
/// between two passes: a pass under a narrower window may have dropped mail the band had not
/// reached yet, which a position recorded before it would skip. So an adapter continues the
/// position its cursor recorded only when told to, and otherwise starts the band afresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widening {
    from: MailboxWindows,
    to: MailboxWindows,
    resumes: bool,
}

impl Widening {
    /// The widening from what `from` covers to `to`, or `None` when `to` admits nothing `from`
    /// does not: the two are equal, `to` is narrower, or its floor only moved on.
    #[must_use]
    pub fn new(from: MailboxWindows, to: MailboxWindows) -> Option<Self> {
        (!from.covers(&to)).then_some(Self {
            from,
            to,
            resumes: false,
        })
    }

    /// The same widening, continuing one that was interrupted (builder-style): the adapter
    /// continues from the position its cursor recorded, and the default reopen continues the
    /// fresh pass its cursor belongs to.
    ///
    /// A caller says so only when the scope's last pass was this widening's (or one toward a depth
    /// that covers this one's, from the same covered depth), its band had begun, and nothing ran on
    /// the scope since under a narrower window. `engine-sync`'s orchestrator records exactly that
    /// beside the cursor ([`StampedCursor`](super::StampedCursor)); a host keeping its own record
    /// keeps the same three facts.
    #[must_use]
    pub fn resuming(mut self) -> Self {
        self.resumes = true;
        self
    }

    /// Whether this widening continues one that was interrupted ([`resuming`](Self::resuming)).
    /// When `false`, a position the cursor recorded belongs to another widening and is ignored.
    #[must_use]
    pub fn resumes(&self) -> bool {
        self.resumes
    }

    /// The windows the store covers.
    #[must_use]
    pub fn from(&self) -> &MailboxWindows {
        &self.from
    }

    /// The windows being widened to.
    #[must_use]
    pub fn to(&self) -> &MailboxWindows {
        &self.to
    }

    /// What `to` admits and `from` does not, as listings: the account band first, then each
    /// mailbox band in mailbox order. See the module docs for the rule.
    #[must_use]
    pub fn bands(&self) -> Vec<Band> {
        bands(Some(&self.from), &self.to)
    }

    /// The one band a scope bound to `mailbox` fetches (IMAP, Graph), or `None` when that
    /// mailbox's window gains nothing: `[to.window_for(mailbox), from.window_for(mailbox))`.
    #[must_use]
    pub fn band_for(&self, mailbox: &MailboxId) -> Option<Band> {
        let held = self.from.window_for(mailbox).floor()?;
        Band::between(
            Some(mailbox),
            self.to.window_for(mailbox).floor(),
            Some(held),
        )
    }
}

impl MailboxWindows {
    /// The listings that together enumerate this window: the account from its floor, with no
    /// upper end, then each mailbox held further back from its own floor up to the account's.
    ///
    /// The same rule as [`Widening::bands`], from a store that holds nothing yet, so an adapter
    /// whose scope is the whole account (JMAP, Gmail) builds a fresh pass and a widening from one
    /// implementation.
    #[must_use]
    pub fn bands(&self) -> Vec<Band> {
        bands(None, self)
    }
}

/// The bands `to` admits beyond `held` (`None`: nothing held).
fn bands(held: Option<&MailboxWindows>, to: &MailboxWindows) -> Vec<Band> {
    let mut bands = Vec::new();
    // Where the account band stops: nowhere when nothing is held, and the held floor otherwise;
    // an account held over its whole history gains nothing.
    let account_upper = match held {
        None => Some(None),
        Some(held) => held.account().floor().map(Some),
    };
    if let Some(before) = account_upper {
        bands.extend(Band::between(None, to.account().floor(), before));
    }
    // A mailbox is deepened only beside a bounded account window, so there is always a floor here.
    let Some(account_floor) = to.account().floor() else {
        return bands;
    };
    for (mailbox, window) in to.deepened() {
        let before = match held.map(|held| held.window_for(mailbox).floor()) {
            None => account_floor,
            Some(None) => continue,
            Some(Some(held)) => held.min(account_floor),
        };
        bands.extend(Band::between(Some(mailbox), window.floor(), Some(before)));
    }
    bands
}

#[cfg(test)]
#[path = "widening_tests.rs"]
mod tests;
