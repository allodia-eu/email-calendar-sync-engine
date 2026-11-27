//! ManageSieve (RFC 5804): where an IMAP account keeps its mail rules and its out-of-office reply,
//! as Sieve scripts on a service beside the mailbox.
//!
//! A module of this crate rather than an adapter of its own, because it serves the same account:
//! it dials with the account's TLS connector, asks the account's credential source, signs in
//! over the same SASL mechanisms, and spends from the account's connection budget
//! (`docs/design/filters-and-out-of-office.md`, decision 7). Its endpoint is the host's
//! ([`ImapConfig::with_managesieve`](crate::ImapConfig::with_managesieve)); there is no SRV
//! discovery (RFC 5804 §1.8) for now.
//!
//! - `response` — what a server answers: data lines of strings and atoms, then `OK`, `NO` or `BYE`
//!   with a response code. The server's prose is never kept.
//! - `session` — the greeting, `STARTTLS`, and the commands a rule editor needs, over any stream.
//! - `auth` — `AUTHENTICATE`: SASL `PLAIN` for a password, `OAUTHBEARER` or `XOAUTH2` for a token.
//! - `dial` — TCP, TLS and the credential source, for a signed-in session.
//! - `error` — [`SieveError`](error::SieveError), a refused command by its code alone.
//! - `service` — [`ManageSieve`](service::ManageSieve), the account's service: a session per call,
//!   in a slot of the account's budget.
//! - `state` — which script holds the rules and the reply, and the version a write is guarded by.
//! - `rules`, `vacation` — [`MailRules`] and [`Vacation`] over it; `import` — the importer of both
//!   (the `import` feature).
//! - `reach` — the greeting read in the background once the account connects, which decides whether
//!   the account's providers offer the two; `probe_state` — what it found, and whether one runs.

mod auth;
pub(crate) mod dial;
pub(crate) mod error;
#[cfg(feature = "import")]
mod import;
pub(crate) mod name;
mod probe_state;
pub(crate) mod reach;
pub(crate) mod response;
mod rules;
pub(crate) mod service;
pub(crate) mod session;
mod state;
mod vacation;

use engine_provider::{MailRules, Vacation};
pub(crate) use probe_state::Offer;
pub(crate) use reach::reach;

/// The rules and the reply of an account, as its providers hand them out: one object, since on
/// ManageSieve they are one script with one version.
pub(crate) trait Sieve: MailRules + Vacation {
    /// What the service offers: not known while a probe runs (both offered), else what the last
    /// probe found.
    fn offered(&self) -> Offer;
    /// Asks a later dial to probe (`ImapAccount::invalidate`).
    fn want_probe(&self);
    /// The rules and the reply as an importer (`import`).
    #[cfg(feature = "import")]
    fn importer(&self) -> &dyn engine_provider::import::RulesImporter;
}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Sieve
    for service::ManageSieve<T>
{
    fn offered(&self) -> Offer {
        self.probe.offered()
    }

    fn want_probe(&self) {
        self.probe.want();
    }

    #[cfg(feature = "import")]
    fn importer(&self) -> &dyn engine_provider::import::RulesImporter {
        self
    }
}

#[cfg(test)]
mod live_account_tests;
#[cfg(all(test, feature = "import"))]
mod live_import_tests;
#[cfg(test)]
mod live_provider_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod test_support;
