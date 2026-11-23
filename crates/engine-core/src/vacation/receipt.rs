//! What a write of the reply answers.

use crate::version::VacationVersion;

/// What a write of an out-of-office reply answers once the server has stored it.
///
/// Built with [`new`](Self::new); `#[non_exhaustive]`, so what an import will add (the parts a
/// target kept) breaks no caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacationReceipt {
    /// The version the reply has now, so the next write can be guarded without reading it
    /// again; `None` on a transport that keeps none.
    pub version: Option<VacationVersion>,
    /// What the write did to the account's own mail rules.
    pub rules: RulesEffect,
}

impl VacationReceipt {
    /// A receipt for a reply stored at `version`, having had `rules` as its effect on the
    /// account's rules.
    #[must_use]
    pub fn new(version: Option<VacationVersion>, rules: RulesEffect) -> Self {
        Self { version, rules }
    }
}

/// What writing the reply did to the account's own mail rules.
///
/// Where the server runs the reply as a Sieve script of its own (RFC 9661 §4), turning it on
/// activates that script, and an account runs at most one (RFC 9661 §2.1), so the user's own
/// filter script stops running. Turning the reply off again does not switch it back. Observed
/// on Stalwart: `docs/design/filters-and-out-of-office.md`, "Settled by live tests". A host
/// tells the user, because their filters have stopped without a word from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RulesEffect {
    /// Every rule script that ran before the write still runs, or the transport keeps the
    /// reply apart from the rules.
    Untouched,
    /// A rule script of the account's own that ran before the write runs no longer.
    Deactivated,
    /// The transport cannot see the account's rules, so it cannot say.
    Unknown,
}
