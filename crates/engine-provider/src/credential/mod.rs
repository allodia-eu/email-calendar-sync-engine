//! Credentials, asked for rather than held (`north-star.md`, principle 7;
//! `docs/design/credential-source.md`).
//!
//! An adapter never keeps a credential for the life of its client. It holds a
//! [`CredentialSource`] and asks it each time a credential is about to go out: on every IMAP
//! dial, every SMTP submission and, once the HTTP adapters move onto it, every request. The
//! source is the host's: an OS keychain and a token refresher on a sync host, a token broker on
//! a transfer host, the user's session on a direct host. The engine owns *when* it asks, what it
//! tells the source about where the credential is going ([`CredentialRequest`]), how long it
//! waits for an answer, and what happens when the server refuses the answer
//! ([`with_credential`]).
//!
//! - [`Secret`] holds a password or token in one buffer that is zeroed when its last clone drops,
//!   and never prints it.
//! - [`Credential`] is what a source answers: a password, a bearer token, or an administrator's
//!   password acting as another account.
//! - [`CredentialSubject`] names who a source signs in as, so everything authenticated can be keyed
//!   by it and never shared across subjects.
//! - [`Origin`] and [`CredentialProtocol`] say where the credential is about to go, and over which
//!   protocol.
//! - [`FixedCredential`] is the source for a credential that never changes.

mod ask;
mod secret;
mod source;

pub use ask::{CredentialFailure, permitted, with_credential};
pub use secret::{Credential, Secret};
pub use source::{
    CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject, FixedCredential,
    Origin,
};

#[cfg(test)]
mod ask_tests;
#[cfg(test)]
mod tests;
