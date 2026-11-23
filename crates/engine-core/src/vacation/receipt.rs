//! What a write of the reply answers.

use crate::version::VacationVersion;

/// What a write of an out-of-office reply answers once the server has stored it.
///
/// Built with [`new`](Self::new); `#[non_exhaustive]`, so what an import will add (the parts a
/// target kept) breaks no caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "the receipt says whether the write switched the user's rules off"]
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
/// tells the user, because their filters stop, and stay stopped, without a word from the server.
/// The engine never switches a script back on by itself: which rules run is the user's call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RulesEffect {
    /// Every rule script that ran before the write still runs, or the transport keeps the
    /// reply apart from the rules.
    Untouched,
    /// A rule script of the account's own that ran before the write runs no longer.
    ///
    /// Read from which script is active, which is what a server reports. A server that ran the
    /// user's script from inside the reply's (Sieve `include`, RFC 6609) would still apply it,
    /// and this would overstate the loss; Stalwart does not, and slice 2 of the design note
    /// settles whether the engine's own rules do.
    Deactivated,
    /// The write turned the reply off, and none of the account's own rule scripts runs, though
    /// it has some: a script the reply switched off when it went on stays off. A host offers to
    /// switch the user's rules back on. The transport cannot tell a script the reply switched
    /// off from one the user had left off, so it is an offer, never done for them.
    StillDeactivated,
    /// The transport cannot see the account's rules, so it cannot say.
    Unknown,
}
