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
//! - `rules`, `vacation` — [`MailRules`] and [`Vacation`] over it.
//! - `reach` — the greeting read when the account connects, which decides whether the account's
//!   providers offer the two.

mod auth;
pub(crate) mod dial;
pub(crate) mod error;
pub(crate) mod name;
pub(crate) mod reach;
pub(crate) mod response;
mod rules;
pub(crate) mod service;
pub(crate) mod session;
mod state;
mod vacation;

use engine_provider::{MailRules, Vacation};
pub(crate) use reach::{probe, reach, reprobe};

/// The rules and the reply of an account, as its providers hand them out: one object, since on
/// ManageSieve they are one script with one version.
pub(crate) trait Sieve: MailRules + Vacation {
    /// Whether the last probe of the service succeeded: the rules are offered only then.
    fn available(&self) -> bool;
    /// Whether the server listed `vacation` then: the reply is offered only then.
    fn replies(&self) -> bool;
    /// Asks for a probe on the account's next dial (`ImapAccount::invalidate`).
    fn mark_stale(&self);
}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Sieve
    for service::ManageSieve<T>
{
    fn available(&self) -> bool {
        self.available()
    }

    fn replies(&self) -> bool {
        self.lists_vacation()
    }

    fn mark_stale(&self) {
        self.mark_stale();
    }
}

#[cfg(test)]
mod live_account_tests;
#[cfg(test)]
mod live_provider_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod test_support;
