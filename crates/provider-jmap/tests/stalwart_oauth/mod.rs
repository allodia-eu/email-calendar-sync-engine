//! Tokens from the Stalwart harness's own OAuth endpoints, for the live suites that present a
//! bearer token: a client registered by RFC 7591, an authorization code with PKCE (RFC 7636)
//! exchanged for an access token, and RFC 7009 revocation.
//!
//! A user signs in on Stalwart's login page; a test cannot, so [`Client::mint_token`] posts that
//! page's own form (`/api/auth`), as the sync host's live OAuth suite does. The harness serves
//! all of it on its plaintext loopback port, under the issuer `https://mail.test.local` its
//! metadata names, so nothing in the harness is changed for it.

use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The scope a token for mail is minted with (Stalwart's metadata lists it). A token minted
/// without one signs in to a session that names no mail account.
const MAIL_SCOPE: &str = "urn:ietf:params:oauth:scope:mail";

/// The scope a token for calendars is minted with. A mail token signs in to CalDAV discovery,
/// but the calendar home answers it `403`. Used by `provider-caldav`'s live suite, which
/// includes this file.
#[allow(dead_code)]
pub(crate) const CALENDARS_SCOPE: &str = "urn:ietf:params:oauth:scope:calendars";

/// Where the code would be sent back to. Nothing dereferences it: the code is read out of the
/// redirect Stalwart answers with.
const REDIRECT_URI: &str = "engine.test://jmap";

fn http() -> reqwest::Client {
    engine_http::client(
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::test_server::user_agent(),
    )
    .build()
    .expect("an HTTP client")
}

/// The `WWW-Authenticate` of the `401` the session answers `authorization` with.
pub(crate) async fn session_challenge(addr: &str, authorization: &str) -> String {
    let reply = http()
        .get(format!("http://{addr}/jmap/session"))
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .await
        .expect("the session answers");
    assert_eq!(reply.status().as_u16(), 401, "the credential is refused");
    reply
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A public OAuth client registered with the harness.
pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    id: String,
    /// What every token this client mints is for.
    scope: &'static str,
}

impl Client {
    /// Registers a public client (no secret) that may redirect to [`REDIRECT_URI`] and ask for
    /// [`MAIL_SCOPE`].
    pub(crate) async fn register(addr: &str) -> Self {
        Self::register_for(addr, MAIL_SCOPE).await
    }

    /// Registers a public client, as [`register`](Self::register) does, whose tokens are for
    /// `scope`.
    #[allow(dead_code)] // Called directly by `provider-caldav`'s live suite only.
    pub(crate) async fn register_for(addr: &str, scope: &'static str) -> Self {
        let http = http();
        let base = format!("http://{addr}");
        let registered: Value = http
            .post(format!("{base}/auth/register"))
            .json(&json!({
                "redirect_uris": [REDIRECT_URI],
                "client_name": "engine live suite",
                "token_endpoint_auth_method": "none",
                "scope": scope,
            }))
            .send()
            .await
            .expect("registration answers")
            .json()
            .await
            .expect("registration is JSON");
        let id = registered["client_id"]
            .as_str()
            .expect("a client id")
            .to_owned();
        Self {
            http,
            base,
            id,
            scope,
        }
    }

    /// Signs `account` in with `password` and returns the access token its code is exchanged
    /// for.
    pub(crate) async fn mint_token(&self, account: &str, password: &str) -> String {
        let verifier = verifier();
        let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
        let authorize = Url::parse_with_params(
            "x:/",
            [
                ("response_type", "code"),
                ("client_id", self.id.as_str()),
                ("redirect_uri", REDIRECT_URI),
                ("state", "live"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("scope", self.scope),
            ],
        )
        .expect("a query");
        let query = authorize.query().expect("a query");
        let mut answer = self
            .login(json!({
                "type": "authCode",
                "accountName": account,
                "accountSecret": password,
                "query": query,
            }))
            .await;
        if answer["type"] == "consentRequired" {
            answer = self
                .login(json!({
                    "type": "authConsent",
                    "token": answer["token"],
                    "query": query,
                    "consent": true,
                }))
                .await;
        }
        let location = answer["location"].as_str().unwrap_or_else(|| {
            panic!(
                "the login page did not sign the account in: {}",
                answer["type"]
            )
        });
        let code = Url::parse(location)
            .expect("the redirect parses")
            .query_pairs()
            .find(|(name, _)| name == "code")
            .map(|(_, code)| code.into_owned())
            .expect("the redirect carries a code");
        let token: Value = self
            .form(
                "/auth/token",
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", REDIRECT_URI),
                    ("client_id", &self.id),
                    ("code_verifier", &verifier),
                ],
            )
            .await
            .json()
            .await
            .expect("the token answer is JSON");
        token["access_token"]
            .as_str()
            .expect("an access token")
            .to_owned()
    }

    /// Revokes `token` (RFC 7009), so the server refuses it from now on.
    pub(crate) async fn revoke(&self, token: &str) {
        let reply = self
            .form("/auth/revoke", &[("token", token), ("client_id", &self.id)])
            .await;
        assert!(reply.status().is_success(), "revoked: {}", reply.status());
    }

    async fn login(&self, body: Value) -> Value {
        self.http
            .post(format!("{}/api/auth", self.base))
            .json(&body)
            .send()
            .await
            .expect("the login form answers")
            .json()
            .await
            .expect("the login answer is JSON")
    }

    async fn form(&self, path: &str, fields: &[(&str, &str)]) -> reqwest::Response {
        let body = Url::parse_with_params("x:/", fields).expect("a form");
        self.http
            .post(format!("{}{path}", self.base))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body.query().unwrap_or_default().to_owned())
            .send()
            .await
            .expect("the token endpoint answers")
    }
}

/// A PKCE code verifier: unreserved characters, 43 or more (RFC 7636 §4.1), unique to the run.
fn verifier() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    format!("live-verifier-{nanos:040}")
}

/// Base64url without padding (RFC 4648 §5), as PKCE's `S256` wants the digest.
fn base64url(bytes: &[u8]) -> String {
    stalwart_harness::base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}
