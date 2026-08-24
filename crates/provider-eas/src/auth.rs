// SPDX-License-Identifier: MPL-2.0
//! EAS authentication strategies. The transport (`client.rs`) calls
//! `auth.authorization_header().await` to populate the `Authorization` header.
//!
//! `Basic` is the historical default. `OAuth` is required for Exchange Online
//! modern-auth tenants. OAuth token storage, caching, expiry tracking, and
//! IdP refresh are deliberately NOT in this crate: the host application
//! supplies a [`TokenProvider`] and the crate just asks it for a token. This
//! keeps `provider-eas` free of kylins' `crate::oauth` / keyring / DB
//! dependencies. Kylins' implementation is `KylinsTokenProvider`
//! (`src/provider/eas/token_provider.rs`).

use std::{fmt, sync::Arc};

use base64::Engine;

use crate::client::EasError;

/// Supplies OAuth access tokens to the EAS transport.
///
/// Implemented by the host application. The implementation owns all token
/// policy: where tokens are stored, when they are considered stale, and how a
/// refresh is performed. `Send + Sync` because the transport shares one
/// `EasAuth` across concurrent requests.
///
/// Failures are reported as [`EasError::Auth`]. The temporary standalone
/// `AuthError` was merged into the shared crate error type so the transport can
/// surface IdP and refresh failures directly to callers.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync {
    /// Return a currently-valid access token. Implementations SHOULD return a
    /// cached token when one is still usable and only hit the IdP when the
    /// cache is absent/expired — this is called once per request.
    async fn access_token(&self) -> Result<String, EasError>;

    /// Force a refresh, bypassing any cache. The transport calls this on a
    /// 401 (see `status::recovery_action_for_http` → `RefreshToken`) before
    /// retrying the request once. Default: delegate to [`Self::access_token`]
    /// (correct for providers that already validate freshness per call).
    async fn force_refresh(&self) -> Result<String, EasError> {
        self.access_token().await
    }
}

/// EAS authentication strategy.
///
/// Clone: `OAuth` holds an `Arc<dyn TokenProvider>`, so cloning shares the
/// provider (and its token cache) rather than duplicating it.
///
/// Serde: the `OAuth` variant is `#[serde(skip)]` — a live token provider is
/// a runtime object and cannot round-trip through config JSON. Serializing an
/// `OAuth` value is an error; deserializing can only produce `Basic`. Config
/// structs holding `Option<EasAuth>` (e.g. `types::EasConfig`) therefore keep
/// their derives, but an OAuth config must be rebuilt at runtime, never
/// persisted.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum EasAuth {
    Basic {
        username: String,
        password: String,
    },
    /// OAuth via a host-supplied token provider. Wrap in `Arc` so `EasAuth`
    /// stays `Clone`; construct via [`EasAuth::oauth`].
    #[serde(skip)]
    OAuth(Arc<dyn TokenProvider>),
}

impl EasAuth {
    /// Basic-auth convenience constructor.
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        EasAuth::Basic {
            username: username.into(),
            password: password.into(),
        }
    }

    /// OAuth constructor: takes ownership of the host's token provider.
    pub fn oauth(provider: Box<dyn TokenProvider>) -> Self {
        EasAuth::OAuth(Arc::from(provider))
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, EasAuth::OAuth(_))
    }

    /// Build the `Authorization` header value for the next request.
    ///
    /// Async because the OAuth branch pulls a token from the provider (which
    /// may refresh against the IdP). Basic never fails; OAuth propagates the
    /// provider's [`EasError::Auth`].
    pub async fn authorization_header(&self) -> Result<String, EasError> {
        match self {
            EasAuth::Basic { username, password } => {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", username, password));
                Ok(format!("Basic {}", encoded))
            }
            EasAuth::OAuth(provider) => {
                let token = provider.access_token().await?;
                Ok(format!("Bearer {}", token))
            }
        }
    }

    /// Force a token refresh (OAuth only; Basic is a no-op success).
    ///
    /// Called by the transport's 401 retry path. The refreshed token is NOT
    /// returned here — the transport rebuilds the header via
    /// [`Self::authorization_header`] after this succeeds, so the provider is
    /// expected to have updated its cache.
    pub async fn refresh(&self) -> Result<(), EasError> {
        match self {
            EasAuth::Basic { .. } => Ok(()),
            EasAuth::OAuth(provider) => provider.force_refresh().await.map(|_| ()),
        }
    }
}

/// Manual `Debug` so the Basic password is never logged and the trait-object
/// provider (which has no `Debug`) doesn't block the derive on config structs.
impl fmt::Debug for EasAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EasAuth::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            EasAuth::OAuth(_) => f.debug_tuple("OAuth").field(&"<token provider>").finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct StaticToken(&'static str);

    #[async_trait::async_trait]
    impl TokenProvider for StaticToken {
        async fn access_token(&self) -> Result<String, EasError> {
            Ok(self.0.to_string())
        }
    }

    #[tokio::test]
    async fn oauth_auth_pulls_token_from_provider() {
        let auth = EasAuth::oauth(Box::new(StaticToken("tok-123")));
        let header = auth.authorization_header().await.unwrap();
        assert_eq!(header, "Bearer tok-123");
        assert!(auth.is_oauth());
    }

    #[tokio::test]
    async fn basic_authorization_header_is_base64() {
        let auth = EasAuth::basic("alice", "s3cret");
        let header = auth.authorization_header().await.unwrap();
        assert_eq!(header, "Basic YWxpY2U6czNjcmV0");
        assert!(!auth.is_oauth());
    }

    #[tokio::test]
    async fn provider_error_propagates_as_auth_error() {
        struct Failing;
        #[async_trait::async_trait]
        impl TokenProvider for Failing {
            async fn access_token(&self) -> Result<String, EasError> {
                Err(EasError::Auth("idp down".into()))
            }
        }
        let auth = EasAuth::oauth(Box::new(Failing));
        let err = auth.authorization_header().await.unwrap_err();
        assert!(matches!(err, EasError::Auth(_)));
        assert!(err.to_string().contains("idp down"));
    }

    #[tokio::test]
    async fn force_refresh_default_delegates_to_access_token() {
        struct Counting(AtomicUsize);
        #[async_trait::async_trait]
        impl TokenProvider for Counting {
            async fn access_token(&self) -> Result<String, EasError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok("tok".into())
            }
        }
        let auth = EasAuth::oauth(Box::new(Counting(AtomicUsize::new(0))));
        auth.refresh().await.unwrap();
        // Default force_refresh → access_token, i.e. exactly one provider call.
        let header = auth.authorization_header().await.unwrap();
        assert_eq!(header, "Bearer tok");
    }

    #[tokio::test]
    async fn refresh_uses_force_refresh_override() {
        struct Refreshing(AtomicUsize);
        #[async_trait::async_trait]
        impl TokenProvider for Refreshing {
            async fn access_token(&self) -> Result<String, EasError> {
                Ok("cached".into())
            }

            async fn force_refresh(&self) -> Result<String, EasError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok("fresh".into())
            }
        }
        let auth = EasAuth::oauth(Box::new(Refreshing(AtomicUsize::new(0))));
        auth.refresh().await.unwrap();
        // refresh() must have hit force_refresh (not access_token).
        assert_eq!(auth.authorization_header().await.unwrap(), "Bearer cached");
    }

    #[tokio::test]
    async fn basic_refresh_is_noop() {
        let auth = EasAuth::basic("u", "p");
        auth.refresh().await.unwrap();
        assert_eq!(auth.authorization_header().await.unwrap(), "Basic dTpw");
    }

    #[test]
    fn oauth_clone_shares_provider() {
        // Arc-backed: cloning an OAuth EasAuth must not require Clone on the
        // provider, and both clones hit the same provider instance.
        let auth = EasAuth::oauth(Box::new(StaticToken("tok-123")));
        let cloned = auth.clone();
        assert!(cloned.is_oauth());
    }

    #[test]
    fn basic_auth_serde_round_trip() {
        let auth = EasAuth::basic("alice", "s3cret");
        let json = serde_json::to_string(&auth).unwrap();
        let back: EasAuth = serde_json::from_str(&json).unwrap();
        assert!(!back.is_oauth());
    }

    #[test]
    fn oauth_variant_is_not_serializable() {
        // `#[serde(skip)]` on the variant: serializing a live provider is a
        // runtime error, not a silent token leak into config JSON.
        let auth = EasAuth::oauth(Box::new(StaticToken("tok-123")));
        assert!(serde_json::to_string(&auth).is_err());
    }

    #[test]
    fn debug_redacts_password_and_provider() {
        let basic = EasAuth::basic("alice", "s3cret");
        let dbg = format!("{:?}", basic);
        assert!(dbg.contains("alice"));
        assert!(
            !dbg.contains("s3cret"),
            "password must be redacted: {}",
            dbg
        );
        let oauth = EasAuth::oauth(Box::new(StaticToken("tok-123")));
        assert!(!format!("{:?}", oauth).contains("tok-123"));
    }
}
