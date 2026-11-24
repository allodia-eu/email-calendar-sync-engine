//! Gated live: JMAP asks the account's credential source on every request, and a credential
//! the server refuses is replaced once and the request sent again with the replacement, against
//! the Stalwart harness.
//!
//! The offline suite (`src/credential_tests.rs`) asserts what each request carried; only the
//! server says that the replayed request is accepted, what its `401` offered, and that a token it
//! minted and then revoked is refused while its successor passes. Each claim has its control:
//!
//! - a wrong password: Stalwart's `401`, read directly, offers Basic (beside Bearer), so the
//!   password was judged and the transport does not re-frame it; the same source without a
//!   replacement fails as `Authentication`, and with one the session and a later method call pass;
//! - a revoked token: refused as Bearer, and replaced by a token minted through Stalwart's own
//!   OAuth endpoints (RFC 7591 registration, the authorization code with PKCE, RFC 7009
//!   revocation).
//!
//! **Failed sign-ins:** five a run (the wrong password read directly, without a replacement and
//! with one; the revoked token read directly and through the client), against Stalwart's ban at
//! 100 a day per IP (`stalwart-harness.md`). Space repeated runs out.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_credential_source -- --nocapture
//! ```

mod stalwart_oauth;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, ProviderError,
};
use provider_jmap::{JmapClient, JmapConfig, Request, capability};
use serde_json::json;
use stalwart_harness::Harness;

/// A source that answers its current credential, and on a refusal its replacement once.
#[derive(Debug)]
struct Replacing {
    subject: CredentialSubject,
    current: Mutex<Credential>,
    replacement: Mutex<Option<Credential>>,
    asks: Mutex<usize>,
    replaced: Mutex<usize>,
}

impl Replacing {
    fn new(first: Credential, replacement: Option<Credential>) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("live-credential-source"),
            current: Mutex::new(first),
            replacement: Mutex::new(replacement),
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        })
    }

    fn counts(&self) -> (usize, usize) {
        (*self.asks.lock().unwrap(), *self.replaced.lock().unwrap())
    }
}

#[async_trait]
impl CredentialSource for Replacing {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        *self.asks.lock().unwrap() += 1;
        Ok(self.current.lock().unwrap().clone())
    }

    async fn replace(
        &self,
        _refused: &Credential,
        _request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        *self.replaced.lock().unwrap() += 1;
        let next = self.replacement.lock().unwrap().take();
        if let Some(next) = &next {
            next.clone_into(&mut self.current.lock().unwrap());
        }
        Ok(next)
    }
}

fn harness() -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_credential_source: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

fn config(harness: &Harness, source: &Arc<Replacing>) -> JmapConfig {
    JmapConfig::from_credential_source(
        format!("http://{}", harness.http_addr),
        Arc::clone(source) as Arc<dyn CredentialSource>,
        engine_http::test_server::user_agent(),
    )
}

/// A method call that needs the account: proves the client is still signed in.
async fn mailboxes(client: &JmapClient) {
    let account = client.session().mail_account_id().expect("a mail account");
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    let call = request.invoke(
        "Mailbox/get",
        json!({ "accountId": account, "ids": null, "properties": ["id"] }),
    );
    let response = client
        .execute(&request)
        .await
        .expect("the call is answered");
    assert!(
        response.result(&call).expect("a Mailbox/get result")["list"].is_array(),
        "the account's mailboxes are listed"
    );
}

const WRONG_PASSWORD: &str = "not-the-harness-password";

#[tokio::test]
async fn live_a_wrong_password_is_replaced_and_the_request_sent_again() {
    let Some(harness) = harness() else { return };

    // What Stalwart's 401 to a wrong password offers, read directly: two challenges, Bearer
    // and Basic. Basic is the scheme the password went out under, so the 401 refuses the
    // password: the transport asks for a replacement and does not re-frame it as a token.
    let challenge = stalwart_oauth::session_challenge(
        &harness.http_addr,
        &format!(
            "Basic {}",
            stalwart_harness::base64_encode(
                format!("{}:{WRONG_PASSWORD}", harness.account).as_bytes()
            )
        ),
    )
    .await;
    assert!(
        challenge.to_ascii_lowercase().contains("basic realm="),
        "the 401 offers the scheme used: {challenge}"
    );

    let wrong = || Credential::password(&harness.account, WRONG_PASSWORD);

    // Control: without a replacement the wrong password fails the connect.
    let unreplaced = Replacing::new(wrong(), None);
    let Err(err) = JmapClient::connect(config(&harness, &unreplaced)).await else {
        panic!("a wrong password must not sign in");
    };
    assert_eq!(
        ProviderError::from(err).class(),
        FailureClass::Authentication
    );
    // One ask a request: the well-known hop, then the session.
    assert_eq!(unreplaced.counts(), (2, 1), "asked, and asked to replace");

    let source = Replacing::new(
        wrong(),
        Some(Credential::password(&harness.account, &harness.password)),
    );
    let client = JmapClient::connect(config(&harness, &source))
        .await
        .expect("the replacement signs in");
    assert_eq!(source.counts(), (2, 1), "an ask a request, one replacement");

    // The next request asks again, and is answered with the replacement.
    mailboxes(&client).await;
    assert_eq!(source.counts(), (3, 1));
}

#[tokio::test]
async fn live_a_revoked_token_is_replaced_by_a_fresh_one() {
    let Some(harness) = harness() else { return };
    let oauth = stalwart_oauth::Client::register(&harness.http_addr).await;
    let revoked = oauth.mint_token(&harness.account, &harness.password).await;
    let fresh = oauth.mint_token(&harness.account, &harness.password).await;
    oauth.revoke(&revoked).await;
    let challenge =
        stalwart_oauth::session_challenge(&harness.http_addr, &format!("Bearer {revoked}")).await;
    // The 401 offers Bearer, the scheme the token went out under, so it refuses the token and
    // the transport asks for a replacement; one offering only Basic would be final.
    assert!(
        challenge.to_ascii_lowercase().contains("bearer realm="),
        "the 401 offers the scheme used: {challenge}"
    );

    let source = Replacing::new(Credential::bearer(revoked), Some(Credential::bearer(fresh)));
    let client = JmapClient::connect(config(&harness, &source))
        .await
        .expect("the fresh token signs in");
    assert_eq!(source.counts(), (2, 1), "an ask a request, one replacement");
    mailboxes(&client).await;
    assert_eq!(source.counts(), (3, 1));
}
