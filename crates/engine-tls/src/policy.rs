//! The host-facing TLS trust policy.

use rustls::pki_types::CertificateDer;

/// Which certificate authorities a client trusts — a small, provider-neutral
/// declaration a host builds once per account and threads into every provider.
///
/// The type is deliberately `rustls`-free apart from the DER newtype, so it maps
/// cleanly across the FFI boundary (an enum plus certificate bytes). Construct it
/// with the presets ([`TlsPolicy::bundled`], [`TlsPolicy::bundled_and_system`], …)
/// or the general [`TlsPolicy::roots`] constructor.
///
/// See `docs/agent-guidance/tls.md` for the trust model and per-platform guidance.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TlsPolicy {
    /// Verify with rustls's own webpki verifier against the **union** of the enabled
    /// root sources. No OS-verifier delegation, so this needs no Android `Context`
    /// and does not inherit any platform revocation behavior.
    Roots {
        /// Include the bundled Mozilla root program ([`webpki_roots`](webpki_roots)).
        bundled: bool,
        /// Include the OS trust store via `rustls-native-certs` (feature
        /// `tls-native-certs`). On Android this sees system CAs but not
        /// user/MDM-installed ones — use [`TlsPolicy::PlatformVerifier`] for those.
        system: bool,
        /// Additional explicit roots, e.g. a private/enterprise CA. Composes with
        /// `bundled`/`system`, or use it alone (via [`TlsPolicy::pinned`]) to trust
        /// exactly these and nothing else.
        custom: Vec<CertificateDer<'static>>,
    },
    /// Delegate the whole trust decision to the OS verifier via
    /// `rustls-platform-verifier` (feature `tls-platform-verifier`). Faithful to the
    /// platform's policy — including Android network-security-config / MDM CAs — but
    /// on Android the host must initialize the JVM `Context` first.
    PlatformVerifier {
        /// Roots trusted in addition to whatever the OS trusts.
        extra_roots: Vec<CertificateDer<'static>>,
    },
}

impl TlsPolicy {
    /// A [`TlsPolicy::Roots`] policy over the given sources — the general
    /// constructor the presets delegate to.
    #[must_use]
    pub fn roots(bundled: bool, system: bool, custom: Vec<CertificateDer<'static>>) -> Self {
        Self::Roots {
            bundled,
            system,
            custom,
        }
    }

    /// Trust only the bundled Mozilla roots — hermetic and identical everywhere.
    /// The engine library default.
    #[must_use]
    pub fn bundled() -> Self {
        Self::roots(true, false, Vec::new())
    }

    /// Trust the bundled Mozilla roots **and** the OS store (the Firefox model) —
    /// the recommended default for native clients, so OS/enterprise CAs are picked
    /// up with zero configuration. Needs the `tls-native-certs` feature.
    #[must_use]
    pub fn bundled_and_system() -> Self {
        Self::roots(true, true, Vec::new())
    }

    /// Trust only the OS store (no bundled roots). Needs the `tls-native-certs`
    /// feature.
    #[must_use]
    pub fn system_only() -> Self {
        Self::roots(false, true, Vec::new())
    }

    /// Trust exactly `roots` and nothing else — the deterministic path for a
    /// regulated deployment pinning its own CA.
    #[must_use]
    pub fn pinned(roots: Vec<CertificateDer<'static>>) -> Self {
        Self::roots(false, false, roots)
    }

    /// Delegate to the OS verifier. Needs the `tls-platform-verifier` feature.
    #[must_use]
    pub fn platform_verifier() -> Self {
        Self::PlatformVerifier {
            extra_roots: Vec::new(),
        }
    }
}

impl Default for TlsPolicy {
    /// The hermetic bundled-roots default (see [`TlsPolicy::bundled`]).
    fn default() -> Self {
        Self::bundled()
    }
}
