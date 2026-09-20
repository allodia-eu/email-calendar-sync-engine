//! `engine-tls` — one TLS trust policy for every provider.
//!
//! The engine speaks TLS two ways: the hand-rolled IMAP/SMTP transport over
//! [`tokio_rustls`], and the HTTP providers (CalDAV/JMAP/Graph) over `reqwest`.
//! Left to their defaults these disagree on *what to trust* — bundled roots on one
//! side, the OS verifier on the other — and on which crypto backend to use. This
//! crate removes that split: a host declares one provider-neutral [`TlsPolicy`],
//! [`client_config`] realizes it into a single ring-backed [`rustls::ClientConfig`],
//! and [`TlsClientConfig`] hands the same trust decision to every provider as a
//! [`tokio_rustls::TlsConnector`] (IMAP/SMTP) or a preconfigured `reqwest` client
//! (the HTTP providers).
//!
//! # Trust model
//!
//! The default is the **bundled Mozilla root program** ([`webpki_roots`]) —
//! hermetic and identical on every platform. A host may instead (or additionally)
//! trust the OS store, mirroring Firefox's *bundled ∪ OS* union, or pin an explicit
//! set of roots. See `docs/agent-guidance/tls.md` for the full policy and the
//! per-platform guidance.
//!
//! # Crypto provider
//!
//! Every config is built with an explicit [`ring`](rustls::crypto::ring) provider,
//! so the workspace carries one crypto backend rather than mixing `ring` and
//! `aws-lc-rs`.
//!
//! # Certificate exceptions
//!
//! A server whose certificate no root can validate — a self-signed CA served as its
//! own end-entity certificate, the shape Proton Mail Bridge and several self-hosted
//! servers use — is reachable only by accepting that one certificate, which
//! [`client_config_with_exceptions`] does and [`CertificateException`] describes.
//! Verification is never relaxed: an exception is consulted only after it has failed,
//! and every refusal is recorded in [`TlsClientConfig::rejected`] so a host can show
//! what it declined rather than only that something was wrong.

mod config;
mod error;
mod exception;
mod policy;
mod summary;

#[cfg(feature = "dangerous-testing")]
mod dangerous;

pub use config::{TlsClientConfig, client_config, client_config_with_exceptions};
pub use error::TlsError;
pub use exception::{CertificateException, RejectedCertificate, fingerprint};
pub use policy::TlsPolicy;
/// A DER-encoded certificate — the currency for [`TlsPolicy`]'s explicit roots.
///
/// Re-exported so hosts and the FFI shim can build a policy from raw certificate
/// bytes without depending on `rustls` directly.
pub use rustls::pki_types::CertificateDer;
pub use summary::CertificateSummary;
