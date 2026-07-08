//! The error type for realizing a [`TlsPolicy`](crate::TlsPolicy) into a config.

use thiserror::Error;

/// A failure building a [`TlsClientConfig`](crate::TlsClientConfig) from a
/// [`TlsPolicy`](crate::TlsPolicy).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TlsError {
    /// The policy selected no trust anchors at all (e.g. an empty `custom` set with
    /// `bundled` and `system` both off). A client with an empty root store would
    /// reject every certificate, so this is rejected up front.
    #[error("the TLS policy selected no root certificates")]
    EmptyRootStore,

    /// An explicit `custom` anchor was not a valid certificate.
    #[error("invalid trust anchor: {0}")]
    InvalidAnchor(String),

    /// The rustls crypto provider could not be initialized for the requested
    /// protocol versions.
    #[error("the rustls crypto provider could not be initialized")]
    Provider,

    /// The platform certificate verifier could not be initialized (on Android this
    /// usually means the host has not initialized the JVM `Context` yet).
    #[error("the platform certificate verifier could not be initialized: {0}")]
    PlatformVerifier(String),

    /// The policy asked for a trust mechanism this build was not compiled with (the
    /// corresponding `engine-tls` feature is off).
    #[error("this build was compiled without support for {0}")]
    Unsupported(&'static str),
}
