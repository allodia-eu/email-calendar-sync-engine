//! Gated live: CalDAV, CardDAV and Talk ask the account's credential source on every request
//! that carries the credential, and a credential the server refuses is replaced once and the
//! request sent again with the replacement, against Stalwart, SabreDAV and Nextcloud.
//!
//! The offline suite (`src/credential_source_tests.rs`) asserts what each request carried; only
//! a server says that the replayed request is accepted and what its `401` offered. Each claim has
//! its control:
//!
//! - a wrong password at connect: the challenge is answered with it, refused, and replaced;
//! - a password that stops working after connect, on an origin that already answered: the next
//!   request carries it unasked, is refused, and is replaced; without a replacement the same
//!   request fails as `Authentication`;
//! - a revoked token (Stalwart): refused as Bearer, and replaced by a token minted through
//!   Stalwart's own OAuth endpoints (RFC 7591 registration, the authorization code with PKCE, RFC
//!   7009 revocation);
//! - `permits` limited to the harness's host: discovery runs in full; a source that permits nothing
//!   fails the connect as `Permanent`.
//!
//! **Failed sign-ins on Stalwart:** six a run, against its ban at 100 a day per IP
//! (`stalwart-harness.md`). Space repeated runs out.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 SABREDAV_HTTP_ADDR=127.0.0.1:18081 \
//!   NEXTCLOUD_HTTP_ADDR=127.0.0.1:18082 \
//!   cargo test -p provider-caldav --test live_credential_source -- --nocapture
//! ```

#[allow(dead_code)] // The JMAP suite's helper; this suite mints and revokes only.
#[path = "../../provider-jmap/tests/stalwart_oauth/mod.rs"]
mod stalwart_oauth;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::{
    CalendarSync, Credential, CredentialRequest, CredentialSource, CredentialSubject,
    MeetingService, Origin, ProviderError, ProviderResult,
};
use provider_caldav::{CalDavConfig, CalDavProvider, CardDavConfig, CardDavProvider, TalkMeetings};

/// A source that answers its current credential and, on a refusal, its replacement once; it
/// permits the hosts in `hosts`, or every host when that is `None`.
#[derive(Debug)]
struct Replacing {
    subject: CredentialSubject,
    current: Mutex<Credential>,
    replacement: Mutex<Option<Credential>>,
    hosts: Option<Vec<String>>,
    asks: Mutex<usize>,
    replaced: Mutex<usize>,
}

impl Replacing {
    fn new(first: Credential, replacement: Option<Credential>) -> Arc<Self> {
        Arc::new(Self::unshared(first, replacement))
    }

    fn unshared(first: Credential, replacement: Option<Credential>) -> Self {
        Self {
            subject: CredentialSubject::new("live-dav-credential-source"),
            current: Mutex::new(first),
            replacement: Mutex::new(replacement),
            hosts: None,
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        }
    }

    /// From now on the source answers `current`, and replaces it with `replacement`.
    fn rotate(&self, current: Credential, replacement: Option<Credential>) {
        *self.current.lock().unwrap() = current;
        *self.replacement.lock().unwrap() = replacement;
    }

    fn replaced(&self) -> usize {
        *self.replaced.lock().unwrap()
    }
}

#[async_trait]
impl CredentialSource for Replacing {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, origin: &Origin) -> bool {
        self.hosts
            .as_ref()
            .is_none_or(|hosts| hosts.iter().any(|host| host == origin.host()))
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        *self.asks.lock().unwrap() += 1;
        Ok(self.current.lock().unwrap().clone())
    }

    async fn replace(
        &self,
        _: &Credential,
        _: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        *self.replaced.lock().unwrap() += 1;
        let next = self.replacement.lock().unwrap().take();
        if let Some(next) = &next {
            *self.current.lock().unwrap() = next.clone();
        }
        Ok(next)
    }
}

/// Lists the account's calendars: one authenticated `PROPFIND` of the home, whose failure
/// comes back as it is.
async fn calendars(provider: &CalDavProvider) -> ProviderResult<()> {
    let account = AccountId::try_from("live-dav-credential").unwrap();
    provider.sync_calendars(&account, None).await.map(|_| ())
}

/// One harness server: its base URL and its account's sign-in.
struct Server {
    name: &'static str,
    base: String,
    user: String,
    password: String,
}

impl Server {
    fn right(&self) -> Credential {
        Credential::password(self.user.clone(), self.password.clone())
    }

    fn wrong(&self) -> Credential {
        Credential::password(self.user.clone(), "not-the-password")
    }

    fn caldav(&self, source: &Arc<Replacing>) -> CalDavConfig {
        CalDavConfig::from_credential_source(
            self.base.clone(),
            Arc::clone(source) as Arc<dyn CredentialSource>,
            engine_http::test_server::user_agent(),
        )
    }
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// The harness servers whose address is set; none offline.
fn servers() -> Vec<Server> {
    let mut servers = Vec::new();
    if let Some(harness) = stalwart_harness::Harness::from_env() {
        servers.push(Server {
            name: "Stalwart",
            base: format!("http://{}", harness.http_addr),
            user: harness.account,
            password: harness.password,
        });
    }
    if let Ok(addr) = std::env::var("SABREDAV_HTTP_ADDR") {
        servers.push(Server {
            name: "SabreDAV",
            base: format!("http://{addr}"),
            user: env("SABREDAV_USER", "alice@test.local"),
            password: env("SABREDAV_PASS", "sabredav-alice-pw"),
        });
    }
    if let Ok(addr) = std::env::var("NEXTCLOUD_HTTP_ADDR") {
        servers.push(Server {
            name: "Nextcloud",
            base: format!("http://{addr}"),
            user: env("NEXTCLOUD_USER", "alice"),
            password: env("NEXTCLOUD_PASS", "nextcloud-alice-pw"),
        });
    }
    if servers.is_empty() {
        eprintln!("skipping: no STALWART_HTTP_ADDR, SABREDAV_HTTP_ADDR or NEXTCLOUD_HTTP_ADDR");
    }
    servers
}

#[tokio::test]
async fn live_a_wrong_password_at_connect_is_replaced() {
    for server in servers() {
        let source = Replacing::new(server.wrong(), Some(server.right()));
        let provider = CalDavProvider::connect(server.caldav(&source))
            .await
            .unwrap_or_else(|err| panic!("{}: the replacement signs in: {err}", server.name));
        assert_eq!(source.replaced(), 1, "{}", server.name);
        calendars(&provider)
            .await
            .unwrap_or_else(|err| panic!("{}: a later request passes: {err}", server.name));
        assert_eq!(
            source.replaced(),
            1,
            "{}: no second replacement",
            server.name
        );
    }
}

#[tokio::test]
async fn live_a_password_that_stops_working_is_replaced_on_an_answered_origin() {
    for server in servers() {
        let source = Replacing::new(server.right(), None);
        let provider = CalDavProvider::connect(server.caldav(&source))
            .await
            .unwrap_or_else(|err| panic!("{}: connect: {err}", server.name));

        // Control: the origin already answered, so the wrong password goes unasked and is
        // refused, and without a replacement that is the person's to fix.
        source.rotate(server.wrong(), None);
        let refused = calendars(&provider)
            .await
            .expect_err("the wrong password is refused");
        assert_eq!(
            refused.class(),
            FailureClass::Authentication,
            "{}",
            server.name
        );
        assert_eq!(
            source.replaced(),
            1,
            "{}: asked, with nothing to give",
            server.name
        );

        source.rotate(server.wrong(), Some(server.right()));
        calendars(&provider)
            .await
            .unwrap_or_else(|err| panic!("{}: replaced and sent again: {err}", server.name));
        assert_eq!(source.replaced(), 2, "{}", server.name);
    }
}

#[tokio::test]
async fn live_carddav_replaces_a_wrong_password_at_connect() {
    // The SabreDAV harness serves calendars only.
    for server in servers()
        .into_iter()
        .filter(|server| server.name != "SabreDAV")
    {
        let source = Replacing::new(server.wrong(), Some(server.right()));
        let config = CardDavConfig::from_credential_source(
            server.base.clone(),
            Arc::clone(&source) as Arc<dyn CredentialSource>,
            engine_http::test_server::user_agent(),
        );
        CardDavProvider::connect(config)
            .await
            .unwrap_or_else(|err| panic!("{}: the replacement signs in: {err}", server.name));
        assert_eq!(source.replaced(), 1, "{}", server.name);
    }
}

#[tokio::test]
async fn live_permits_limited_to_the_harness_host_still_discovers_and_none_refuses() {
    for server in servers() {
        let host = reqwest::Url::parse(&server.base)
            .unwrap()
            .host_str()
            .unwrap()
            .to_owned();
        let source = Arc::new(Replacing {
            hosts: Some(vec![host]),
            ..Replacing::unshared(server.right(), None)
        });
        let provider = CalDavProvider::connect(server.caldav(&source))
            .await
            .unwrap_or_else(|err| panic!("{}: discovery within the host: {err}", server.name));
        assert!(provider.principal_url().is_some(), "{}", server.name);

        let nowhere = Arc::new(Replacing {
            hosts: Some(Vec::new()),
            ..Replacing::unshared(server.right(), None)
        });
        let refused = CalDavProvider::connect(server.caldav(&nowhere))
            .await
            .expect_err("a source that permits nothing");
        assert_eq!(
            refused.failure_class(),
            FailureClass::Permanent,
            "{}",
            server.name
        );
        assert_eq!(
            *nowhere.asks.lock().unwrap(),
            0,
            "{}: never asked",
            server.name
        );
    }
}

#[tokio::test]
async fn live_a_revoked_token_is_replaced_by_a_fresh_one_on_stalwart() {
    let Some(harness) = stalwart_harness::Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let oauth =
        stalwart_oauth::Client::register_for(&harness.http_addr, stalwart_oauth::CALENDARS_SCOPE)
            .await;
    let revoked = oauth.mint_token(&harness.account, &harness.password).await;
    let fresh = oauth.mint_token(&harness.account, &harness.password).await;
    oauth.revoke(&revoked).await;
    let config = |source: &Arc<Replacing>| {
        CalDavConfig::from_credential_source(
            format!("http://{}", harness.http_addr),
            Arc::clone(source) as Arc<dyn CredentialSource>,
            engine_http::test_server::user_agent(),
        )
    };

    // Control: the revoked token alone is refused, as the person's to sign in again.
    let alone = Replacing::new(Credential::bearer(revoked.clone()), None);
    let refused = CalDavProvider::connect(config(&alone))
        .await
        .expect_err("a revoked token is refused");
    assert_eq!(refused.failure_class(), FailureClass::Authentication);

    let source = Replacing::new(Credential::bearer(revoked), Some(Credential::bearer(fresh)));
    let provider = CalDavProvider::connect(config(&source))
        .await
        .expect("the fresh token signs in");
    assert_eq!(source.replaced(), 1);
    calendars(&provider).await.expect("and keeps working");
}

#[tokio::test]
async fn live_talk_replaces_a_wrong_password_on_nextcloud() {
    let Some(server) = servers()
        .into_iter()
        .find(|server| server.name == "Nextcloud")
    else {
        return;
    };
    let source = Replacing::new(server.wrong(), Some(server.right()));
    let talk = TalkMeetings::from_credential_source(
        &server.base,
        Arc::clone(&source) as Arc<dyn CredentialSource>,
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("a Talk client");
    let meeting = talk
        .create_meeting(&engine_provider::MeetingRequest {
            key: engine_provider::MeetingKey::new("q4w8e2r6t0y3u7i1o5p9a2s6d0").unwrap(),
            title: "Credential replacement".into(),
            start: "2026-09-14T09:00:00Z".parse().unwrap(),
            end: "2026-09-14T09:30:00Z".parse().unwrap(),
        })
        .await
        .expect("the replacement makes the meeting");
    assert_eq!(source.replaced(), 1);
    talk.delete_meeting(&meeting).await.expect("and removes it");
}
