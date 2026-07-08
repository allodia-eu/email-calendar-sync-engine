//! Realizing a [`TlsPolicy`] into one shared [`rustls::ClientConfig`] and the
//! per-transport currencies built from it.

use std::sync::{Arc, Once};

use rustls::{
    ClientConfig, RootCertStore, client::danger::ServerCertVerifier, crypto::CryptoProvider,
    pki_types::CertificateDer,
};
use tokio_rustls::TlsConnector;

use crate::{TlsError, TlsPolicy};

/// One TLS client configuration, shared by every provider of one account.
///
/// Build it once from a [`TlsPolicy`] with [`client_config`], then hand the same
/// trust decision to each transport: [`TlsClientConfig::connector`] for the
/// hand-rolled IMAP/SMTP stack and [`TlsClientConfig::reqwest_builder`] for the
/// HTTP providers. Cloning is cheap (it shares one `Arc`), so a host builds one and
/// stores a clone in each provider's config.
#[derive(Clone)]
pub struct TlsClientConfig(Arc<ClientConfig>);

impl core::fmt::Debug for TlsClientConfig {
    /// Terse: the wrapped rustls config is large and its contents are not useful in
    /// a provider config's debug output.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TlsClientConfig").finish_non_exhaustive()
    }
}

impl Default for TlsClientConfig {
    /// The hermetic bundled-roots default (see [`TlsClientConfig::bundled`]).
    fn default() -> Self {
        Self::bundled()
    }
}

/// Realizes `policy` into one ring-backed [`rustls::ClientConfig`].
///
/// # Errors
///
/// Returns [`TlsError::EmptyRootStore`] if the policy selects no anchors,
/// [`TlsError::InvalidAnchor`] if an explicit root is not a valid certificate,
/// [`TlsError::PlatformVerifier`] if the OS verifier cannot be initialized, or
/// [`TlsError::Unsupported`] if the policy needs a trust mechanism this build was
/// not compiled with.
pub fn client_config(policy: &TlsPolicy) -> Result<TlsClientConfig, TlsError> {
    ensure_process_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| TlsError::Provider)?;

    let config = match policy {
        TlsPolicy::Roots {
            bundled,
            system,
            custom,
        } => builder
            .with_root_certificates(build_root_store(*bundled, *system, custom)?)
            .with_no_client_auth(),
        TlsPolicy::PlatformVerifier { extra_roots } => builder
            .dangerous()
            .with_custom_certificate_verifier(platform_verifier(&provider, extra_roots)?)
            .with_no_client_auth(),
    };
    Ok(TlsClientConfig(Arc::new(config)))
}

impl TlsClientConfig {
    /// The hermetic bundled-roots config — the engine default, used when a host
    /// does not select a policy.
    ///
    /// Infallible: the bundled Mozilla root set is a non-empty constant and the
    /// `ring` provider is always available, so [`client_config`] cannot fail here.
    #[must_use]
    pub fn bundled() -> Self {
        client_config(&TlsPolicy::bundled()).expect("bundled Mozilla roots are always available")
    }

    /// A [`TlsConnector`] carrying this trust policy — for the IMAP/SMTP transport.
    #[must_use]
    pub fn connector(&self) -> TlsConnector {
        TlsConnector::from(self.0.clone())
    }

    /// A `reqwest` client builder preconfigured with this trust policy — for the
    /// CalDAV/JMAP/Graph providers, which finish it with their own (non-TLS)
    /// settings such as the redirect policy.
    ///
    /// ALPN is set to HTTP/1.1 here: the shared config carries no ALPN (correct for
    /// IMAP/SMTP), and reqwest's preconfigured-TLS path does not set it.
    #[cfg(feature = "reqwest")]
    pub fn reqwest_builder(&self) -> reqwest::ClientBuilder {
        let mut config = (*self.0).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        reqwest::Client::builder().tls_backend_preconfigured(config)
    }

    /// TEST BUILDS ONLY — a config that accepts **any** server certificate, for the
    /// self-signed Stalwart harness. Never compiled into a default build.
    #[cfg(feature = "dangerous-testing")]
    #[doc(hidden)]
    #[must_use]
    pub fn dangerous_accept_any() -> Self {
        ensure_process_provider();
        Self(Arc::new(crate::dangerous::accept_any_config()))
    }
}

/// Assembles one [`RootCertStore`] from the enabled sources (bundled ∪ system ∪
/// custom). Rejects an empty result up front, since a client with no roots would
/// reject every certificate.
pub(crate) fn build_root_store(
    bundled: bool,
    system: bool,
    custom: &[CertificateDer<'static>],
) -> Result<RootCertStore, TlsError> {
    let mut store = RootCertStore::empty();
    if bundled {
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    if system {
        #[cfg(feature = "tls-native-certs")]
        {
            // Best-effort: add whatever the OS store yields and let the final
            // `is_empty` check be the single gate, so a flaky OS store never fails a
            // `bundled`/`custom` union (the recommended client default).
            let result = rustls_native_certs::load_native_certs();
            let _ = store.add_parsable_certificates(result.certs);
        }
        #[cfg(not(feature = "tls-native-certs"))]
        return Err(TlsError::Unsupported("system trust (tls-native-certs)"));
    }
    for der in custom {
        store
            .add(der.clone())
            .map_err(|e| TlsError::InvalidAnchor(e.to_string()))?;
    }
    if store.is_empty() {
        return Err(TlsError::EmptyRootStore);
    }
    Ok(store)
}

#[cfg(feature = "tls-platform-verifier")]
fn platform_verifier(
    provider: &Arc<CryptoProvider>,
    extra_roots: &[CertificateDer<'static>],
) -> Result<Arc<dyn ServerCertVerifier>, TlsError> {
    let verifier = if extra_roots.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
            .map_err(|e| TlsError::PlatformVerifier(e.to_string()))?
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(
            extra_roots.to_vec(),
            provider.clone(),
        )
        .map_err(|e| TlsError::PlatformVerifier(e.to_string()))?
    };
    Ok(Arc::new(verifier))
}

#[cfg(not(feature = "tls-platform-verifier"))]
fn platform_verifier(
    _provider: &Arc<CryptoProvider>,
    _extra_roots: &[CertificateDer<'static>],
) -> Result<Arc<dyn ServerCertVerifier>, TlsError> {
    Err(TlsError::Unsupported("OS verifier (tls-platform-verifier)"))
}

/// Installs a `ring` process-default crypto provider once. Not required by our own
/// code paths (every config names its provider explicitly), but it is cheap
/// insurance against a stray reqwest client built without preconfigured TLS, which
/// would otherwise panic once `aws-lc-rs` is out of the tree.
fn ensure_process_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::CertificateDer;

    use super::{TlsClientConfig, TlsError, TlsPolicy, build_root_store, client_config};

    fn self_signed_der() -> CertificateDer<'static> {
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate self-signed cert")
            .cert
            .der()
            .clone()
    }

    #[test]
    fn bundled_policy_builds() {
        assert!(client_config(&TlsPolicy::bundled()).is_ok());
    }

    #[test]
    fn empty_union_is_rejected() {
        let err = client_config(&TlsPolicy::pinned(Vec::new())).unwrap_err();
        assert!(matches!(err, TlsError::EmptyRootStore));
    }

    #[test]
    fn custom_root_builds_and_counts_one() {
        let der = self_signed_der();
        let store = build_root_store(false, false, std::slice::from_ref(&der)).unwrap();
        assert_eq!(store.len(), 1);
        assert!(client_config(&TlsPolicy::pinned(vec![der])).is_ok());
    }

    #[test]
    fn bundled_has_many_and_custom_adds_one() {
        let der = self_signed_der();
        let bundled = build_root_store(true, false, &[]).unwrap();
        let bundled_plus = build_root_store(true, false, std::slice::from_ref(&der)).unwrap();
        assert!(bundled.len() > 100, "webpki-roots should ship many anchors");
        assert_eq!(bundled_plus.len(), bundled.len() + 1);
    }

    #[test]
    fn invalid_custom_der_errors() {
        let bad = CertificateDer::from(vec![0u8, 1, 2, 3]);
        let err = build_root_store(false, false, std::slice::from_ref(&bad)).unwrap_err();
        assert!(matches!(err, TlsError::InvalidAnchor(_)));
    }

    #[test]
    fn connector_builds_from_bundled() {
        let _connector = client_config(&TlsPolicy::bundled()).unwrap().connector();
    }

    #[test]
    fn presets_construct_and_default_debugs() {
        // Construct every preset (cheap, no I/O) so the policy surface is covered.
        let _ = (
            TlsPolicy::roots(false, false, Vec::new()),
            TlsPolicy::bundled_and_system(),
            TlsPolicy::system_only(),
            TlsPolicy::platform_verifier(),
            TlsPolicy::default(),
        );
        // The built config's Debug is terse and its Default is bundled.
        let cfg = TlsClientConfig::default();
        assert!(format!("{cfg:?}").contains("TlsClientConfig"));
    }

    #[cfg(feature = "reqwest")]
    #[test]
    fn reqwest_builder_builds_from_bundled() {
        let config = client_config(&TlsPolicy::bundled()).unwrap();
        assert!(config.reqwest_builder().build().is_ok());
    }

    #[cfg(not(feature = "tls-native-certs"))]
    #[test]
    fn system_trust_unsupported_without_feature() {
        let err = client_config(&TlsPolicy::system_only()).unwrap_err();
        assert!(matches!(err, TlsError::Unsupported(_)));
    }

    // The bundled ∪ system union is robust even if the OS store is flaky: the
    // bundled roots keep the store non-empty. (Exercises the native-certs path.)
    #[cfg(feature = "tls-native-certs")]
    #[test]
    fn bundled_and_system_builds() {
        assert!(client_config(&TlsPolicy::bundled_and_system()).is_ok());
    }

    // The OS verifier builds regardless of store contents (it queries at verify
    // time), so this is robust on any host.
    #[cfg(feature = "tls-platform-verifier")]
    #[test]
    fn platform_verifier_builds() {
        assert!(client_config(&TlsPolicy::platform_verifier()).is_ok());
    }
}
