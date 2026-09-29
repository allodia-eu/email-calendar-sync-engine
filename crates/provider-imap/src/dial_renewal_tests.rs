//! Offline tests for where a dial gets its credential: asked of the config's
//! [`CredentialSource`] on every dial, and renewed once when the server refuses a token.
//!
//! An access token expires within the hour while an account's pool lives for as long as the
//! host runs, so a pool that kept presenting the token it was built with would lose every new
//! connection an hour in. These pin what the wire carries on each dial, over scripted streams.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{ProviderError, TlsVersion};

use super::dial_with;
use crate::{
    config::ImapConfig,
    credentials::{CredentialSource, Credentials},
    error::ImapError,
    mock::{MockStream, Recorded, script, written},
    pool::{Dial, ImapPool},
    sasl::Mechanism,
    transport::Connection,
};

const USER: &str = "alice@example.com";
const HOST: &str = "imap.example.com";
const GREETING: &str = "* OK ready\r\n";
const SASL_CAPABILITY: &str =
    "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n";
/// A rest threshold no test outlives, so a parked connection is never checked.
const NEVER_CHECK: Duration = Duration::from_hours(1);

/// A server that accepts whatever token it is given.
fn accepting() -> Vec<u8> {
    script(&[
        GREETING,
        SASL_CAPABILITY,
        "a2 OK authenticated\r\n",
        "* CAPABILITY IMAP4rev1 IDLE\r\na3 OK done\r\n",
    ])
}

/// A server that refuses the token, as one does when it has expired.
fn refusing() -> Vec<u8> {
    script(&[
        GREETING,
        SASL_CAPABILITY,
        "a2 NO [AUTHENTICATIONFAILED] token expired\r\n",
    ])
}

/// The `OAUTHBEARER` initial response that carries `token`: what the wire shows when the
/// dial presented it.
fn presented(token: &str) -> String {
    Mechanism::OAuthBearer
        .initial_response(USER, token, HOST, Some(993))
        .expect("clean credential")
}

/// A host's token source: hands out whatever token is current, and on `renew` switches to
/// the refreshed one it was given, if any.
#[derive(Debug, Default)]
struct TokenSource {
    current: Mutex<String>,
    refreshed: Mutex<Option<String>>,
    renewals: AtomicUsize,
}

impl TokenSource {
    fn new(current: &str, refreshed: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(current.to_owned()),
            refreshed: Mutex::new(refreshed.map(str::to_owned)),
            renewals: AtomicUsize::new(0),
        })
    }

    /// What a host's refresh does between two dials: the old token is gone.
    fn rotate(&self, token: &str) {
        token.clone_into(&mut self.current.lock().unwrap());
    }
}

#[async_trait]
impl CredentialSource for TokenSource {
    async fn credentials(&self) -> Result<Credentials, ProviderError> {
        Ok(Credentials::oauth2(
            USER,
            self.current.lock().unwrap().clone(),
        ))
    }

    async fn renew(&self, _refused: &Credentials) -> Result<Option<Credentials>, ProviderError> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        let refreshed = self.refreshed.lock().unwrap().take();
        Ok(refreshed.map(|token| {
            self.rotate(&token);
            Credentials::oauth2(USER, token)
        }))
    }
}

/// A source whose grant is gone: it has no credential to give at all.
#[derive(Debug)]
struct RevokedSource;

#[async_trait]
impl CredentialSource for RevokedSource {
    async fn credentials(&self) -> Result<Credentials, ProviderError> {
        Err(ProviderError::authentication("the grant was revoked"))
    }
}

type Opened = Result<(Connection<MockStream>, Option<TlsVersion>), ImapError>;

/// Scripted servers, handed out one per connection the dial opens, and what the client wrote
/// on each.
#[derive(Clone, Default)]
struct Servers {
    scripts: Arc<Mutex<VecDeque<Vec<u8>>>>,
    sessions: Arc<Mutex<Vec<Recorded>>>,
}

impl Servers {
    fn new(scripts: Vec<Vec<u8>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            sessions: Arc::default(),
        }
    }

    /// Opens the next scripted connection, as `open_secured` would a real one.
    fn open(&self) -> impl Future<Output = Opened> + use<> {
        let server = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("the dial opened more connections than the test scripted");
        let (stream, recorded) = MockStream::new(server);
        self.sessions.lock().unwrap().push(recorded);
        async move { Connection::open(stream).await.map(|c| (c, None)) }
    }

    fn written(&self, session: usize) -> String {
        written(&self.sessions.lock().unwrap()[session])
    }

    fn opened(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    fn unused(&self) -> usize {
        self.scripts.lock().unwrap().len()
    }

    /// The pool's dial over these servers, through the same credential path a live dial takes.
    fn dial(&self, config: ImapConfig) -> Dial<MockStream> {
        let config = Arc::new(config);
        let servers = self.clone();
        Arc::new(move || {
            let config = Arc::clone(&config);
            let servers = servers.clone();
            Box::pin(async move {
                dial_with(&config, || servers.open())
                    .await
                    .map(|(connection, _)| connection)
            })
        })
    }
}

fn token_config(source: Arc<dyn CredentialSource>) -> ImapConfig {
    ImapConfig::from_credential_source(format!("{HOST}:993"), HOST, source)
}

#[tokio::test]
async fn a_pooled_dial_after_the_token_was_refreshed_presents_the_new_one() {
    let source = TokenSource::new("first-token", None);
    let servers = Servers::new(vec![accepting(), accepting()]);
    let pool = ImapPool::new(servers.dial(token_config(source.clone())), 5, NEVER_CHECK);

    let first = pool.acquire().await.expect("first connection");
    // The first token expires and the host refreshes it; the pool's first connection stays
    // open, because a session outlives the token that opened it.
    source.rotate("second-token");
    let second = pool.acquire().await.expect("second connection");

    assert_eq!(servers.opened(), 2);
    assert!(servers.written(0).contains(&presented("first-token")));
    let sent = servers.written(1);
    assert!(
        sent.contains(&presented("second-token")),
        "the second dial must ask the source, not reuse the token the account connected with: \
         {sent}"
    );
    assert!(!sent.contains(&presented("first-token")), "{sent}");
    drop((first, second));
}

#[tokio::test]
async fn a_refused_token_is_renewed_once_and_the_dial_retried_with_it() {
    let source = TokenSource::new("expired-token", Some("refreshed-token"));
    let servers = Servers::new(vec![refusing(), accepting()]);
    let config = token_config(source.clone());

    dial_with(&config, || servers.open())
        .await
        .expect("the renewed token authenticates");

    assert_eq!(source.renewals.load(Ordering::SeqCst), 1);
    assert_eq!(servers.opened(), 2, "one re-dial, on a fresh connection");
    assert!(servers.written(1).contains(&presented("refreshed-token")));
}

#[tokio::test]
async fn an_expired_token_does_not_lower_the_pools_connection_limit() {
    // A refused token that a fresh one then replaces said nothing about how many sessions
    // the server allows, so the pool must not read it as a cap.
    let source = TokenSource::new("expired-token", Some("refreshed-token"));
    let servers = Servers::new(vec![accepting(), refusing(), accepting()]);
    let pool = ImapPool::new(servers.dial(token_config(source.clone())), 3, NEVER_CHECK);

    let held = pool.acquire().await.expect("first connection");
    source.rotate("expired-token");
    // Bounded: a pool that read the refusal as a cap would wait for the held connection
    // rather than fail, and a regression should read as a failure, not a hang.
    let second = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
        .await
        .expect("the caller must be served, not left waiting for the held connection")
        .expect("the renewed dial serves the caller");

    assert_eq!(pool.ceiling(), 3);
    assert_eq!(source.renewals.load(Ordering::SeqCst), 1);
    drop((held, second));
}

#[tokio::test]
async fn a_refused_password_is_not_tried_again() {
    // The same secret to the same server gets the same answer, at a provider that may be
    // counting failures towards a lockout.
    let servers = Servers::new(vec![
        script(&[GREETING, "a1 NO [AUTHENTICATIONFAILED] bad password\r\n"]),
        accepting(),
    ]);
    let config = ImapConfig::new(
        format!("{HOST}:993"),
        HOST,
        Credentials::password(USER, "wrong"),
    );

    let err = dial_with(&config, || servers.open())
        .await
        .expect_err("a refused password must fail");

    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(servers.opened(), 1);
    assert_eq!(servers.unused(), 1);
}

#[tokio::test]
async fn a_renewed_token_refused_again_is_reported_rather_than_renewed_again() {
    let source = TokenSource::new("expired-token", Some("also-refused"));
    let servers = Servers::new(vec![refusing(), refusing(), accepting()]);
    let config = token_config(source.clone());

    let err = dial_with(&config, || servers.open())
        .await
        .expect_err("two refusals must fail");

    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(source.renewals.load(Ordering::SeqCst), 1);
    assert_eq!(servers.opened(), 2);
}

#[tokio::test]
async fn a_source_with_no_credential_fails_with_its_own_class_and_dials_nothing() {
    let servers = Servers::new(vec![accepting()]);
    let config = token_config(Arc::new(RevokedSource));

    let err = dial_with(&config, || servers.open())
        .await
        .expect_err("no credential, no session");

    assert!(matches!(err, ImapError::Credential(_)), "{err:?}");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(
        servers.opened(),
        0,
        "nothing is dialled without a credential"
    );
}
