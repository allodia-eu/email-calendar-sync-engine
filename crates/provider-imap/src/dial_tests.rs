//! Offline tests for the dial — the connect sequence and the steps it reports.
//!
//! Driven over a `MockStream`, so the exact `ConnectStep` order is asserted without a
//! socket: the handshake has already happened by the time [`open_session`] is called,
//! which is why the TLS version is passed in rather than read off the stream.

use engine_provider::TlsVersion;

use super::open_session;
use crate::{
    config::ImapConfig,
    credentials::Credentials,
    mock::{MockStream, script, written},
    sasl::Mechanism,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// Records connect steps as the log lines a host would emit.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl engine_provider::ConnectObserver for Recorder {
    fn step(&self, step: &engine_provider::ConnectStep<'_>) {
        use engine_provider::ConnectStep;
        let line = match step {
            ConnectStep::TlsEstablished(version) => format!("tls {version:?}"),
            ConnectStep::Authenticated => "authenticated".to_owned(),
            ConnectStep::Negotiated {
                dialect, features, ..
            } => {
                format!("negotiated {dialect} [{}]", features.join(" "))
            }
            other => format!("unexpected {other:?}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

/// Drives the shared dial over a mock stream, returning the steps it reported.
async fn observed_open_session(server_script: Vec<u8>, tls: Option<TlsVersion>) -> Vec<String> {
    let recorder = std::sync::Arc::new(Recorder::default());
    let config = ImapConfig::new(
        "h:993",
        "h",
        Credentials::password("alice@test.local", "pw"),
    )
    .with_connect_observer(recorder.clone());
    let (stream, _recorded) = MockStream::new(server_script);
    open_session(stream, tls, &config).await.expect("session");
    let steps = recorder.0.lock().unwrap();
    steps.clone()
}

#[tokio::test]
async fn an_oauth_config_authenticates_over_sasl_instead_of_logging_in() {
    // The dial's credential branch: the same session, reached with a token rather than a
    // password. What this pins is the wiring — that `server_name` and the port parsed
    // from `addr` are what reach the `OAUTHBEARER` response, since a blob naming the
    // wrong host is a rejection no offline test of `sasl` alone would catch.
    let recorder = std::sync::Arc::new(Recorder::default());
    let config = ImapConfig::new(
        "imap.example.com:993",
        "imap.example.com",
        Credentials::oauth2("alice@example.com", "ya29.token"),
    )
    .with_connect_observer(recorder.clone());
    let (stream, wire) = MockStream::new(script(&[
        GREETING,
        "* CAPABILITY IMAP4rev1 SASL-IR AUTH=OAUTHBEARER\r\na1 OK CAPABILITY done\r\n",
        "a2 OK alice@example.com authenticated\r\n",
        "* CAPABILITY IMAP4rev1 IDLE\r\na3 OK done\r\n",
    ]));
    open_session(stream, None, &config).await.expect("session");

    let sent = written(&wire);
    let expected = Mechanism::OAuthBearer
        .initial_response(
            "alice@example.com",
            "ya29.token",
            "imap.example.com",
            Some(993),
        )
        .expect("clean credential");
    assert!(
        sent.contains(&format!("a2 AUTHENTICATE OAUTHBEARER {expected}\r\n")),
        "{sent}"
    );
    assert!(
        !sent.contains("LOGIN"),
        "a token must never be sent as a password: {sent}"
    );
    // The observer is told the session authenticated, not how — a host's connect trace
    // reads the same either way.
    let steps = recorder.0.lock().unwrap().clone();
    assert!(steps.contains(&"authenticated".to_owned()), "{steps:?}");
}

#[tokio::test]
async fn connect_reports_the_tls_handshake_then_the_login() {
    // The exact sequence, in order: the handshake precedes the greeting, and `LOGIN`
    // precedes the post-auth CAPABILITY (which is extension negotiation, not a step).
    let steps = observed_open_session(
        script(&[
            GREETING,
            LOGIN_OK,
            "* CAPABILITY IMAP4rev2 IDLE\r\na2 OK done\r\n",
            // Advertising rev2 makes the session enable it, so the script must answer.
            "* ENABLED IMAP4rev2\r\na3 OK ENABLE done\r\n",
        ]),
        Some(TlsVersion::Tls1_3),
    )
    .await;
    // The dialect step closes the trace, and shows what one ENABLE bought: this server
    // advertised only IDLE, but rev2 folds LIST-STATUS and SPECIAL-USE in too. QRESYNC is
    // absent because it is *not* folded in and this server never offered it — which is
    // exactly the distinction a support session needs the line to make.
    assert_eq!(
        steps,
        [
            "tls Tls1_3",
            "authenticated",
            "negotiated IMAP4rev2 [IDLE LIST-STATUS SPECIAL-USE]"
        ]
    );
}

#[tokio::test]
async fn a_stream_that_is_not_tls_reports_only_the_login() {
    // `tls_version` is `None` when the stream is not TLS — the fact is not applicable,
    // not merely unobserved, so no step is invented for it.
    let steps = observed_open_session(script(&[GREETING, LOGIN_OK, "a2 OK done\r\n"]), None).await;
    // No capability list at all: rev1 baseline, nothing usable, and nothing to enable.
    assert_eq!(steps, ["authenticated", "negotiated IMAP4rev1 []"]);
}

#[tokio::test]
async fn a_failed_login_reports_the_handshake_but_never_authentication() {
    // `Authenticated` means the server accepted the credentials. A `NO` must not emit
    // it — a host driving a state machine off these steps would otherwise believe a
    // rejected connection came up.
    let recorder = std::sync::Arc::new(Recorder::default());
    let config = ImapConfig::new(
        "h:993",
        "h",
        Credentials::password("alice@test.local", "wrong"),
    )
    .with_connect_observer(recorder.clone());
    let (stream, _recorded) = MockStream::new(script(&[GREETING, "a1 NO bad credentials\r\n"]));
    let err = open_session(stream, Some(TlsVersion::Tls1_2), &config)
        .await
        .expect_err("login must fail");
    assert!(matches!(err, crate::error::ImapError::Auth(_)));
    assert_eq!(*recorder.0.lock().unwrap(), ["tls Tls1_2"]);
}
