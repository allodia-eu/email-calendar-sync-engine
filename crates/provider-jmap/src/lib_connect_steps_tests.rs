//! What a connect through the real client reports to its observer: each well-known hop, the
//! authentication and the API URL, and never a credential.

use std::sync::Arc;

use super::*;

/// Records connect steps as the log lines a host would emit, so the assertions below
/// read as "exactly what would have reached the log, in order".
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl engine_provider::ConnectObserver for Recorder {
    fn step(&self, step: &ConnectStep<'_>) {
        let line = match step {
            ConnectStep::Redirected { from, to, .. } => format!("redirected {from} -> {to}"),
            ConnectStep::TlsEstablished(version) => format!("tls {version:?}"),
            ConnectStep::Authenticated => "authenticated".to_owned(),
            ConnectStep::Discovered { endpoint, .. } => format!("discovered {endpoint}"),
            other => format!("unmodeled {other:?}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

impl Recorder {
    fn steps(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn http_redirect(location: &str) -> String {
    format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[tokio::test]
async fn connect_reports_every_well_known_hop_then_auth_then_the_api_url() {
    // Two hops, so the chain is a *sequence* and not a single latched observation:
    // /.well-known/jmap -> /jmap/hop -> /jmap/session -> 200.
    let base = mock_server(vec![
        http_redirect("/jmap/hop"),
        http_redirect("/jmap/session"),
        http_ok(SESSION_DOC),
    ]);
    let recorder = Arc::new(Recorder::default());
    JmapClient::connect(
        JmapConfig::new(
            base.clone(),
            Credentials::basic("a", "b"),
            engine_http::test_server::user_agent(),
        )
        .with_connect_observer(recorder.clone()),
    )
    .await
    .unwrap();

    assert_eq!(
        recorder.steps(),
        [
            format!("redirected {base}/.well-known/jmap -> {base}/jmap/hop"),
            format!("redirected {base}/jmap/hop -> {base}/jmap/session"),
            "authenticated".to_owned(),
            // `SESSION_DOC` advertises https://mail.test.local/jmap/; the default
            // `RebaseToConnection` policy resolves it onto the connection origin.
            format!("discovered {base}/jmap/"),
        ]
    );
}

#[tokio::test]
async fn a_session_served_without_a_redirect_reports_no_hop() {
    let base = mock_server(vec![http_ok(SESSION_DOC)]);
    let recorder = Arc::new(Recorder::default());
    JmapClient::connect(
        JmapConfig::new(
            base.clone(),
            Credentials::basic("a", "b"),
            engine_http::test_server::user_agent(),
        )
        .with_session_path("/jmap/session")
        .with_connect_observer(recorder.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        recorder.steps(),
        [
            "authenticated".to_owned(),
            format!("discovered {base}/jmap/")
        ]
    );
}

#[tokio::test]
async fn a_connect_without_an_observer_is_the_no_op_default() {
    // The seam is additive: the existing whole-config path still connects.
    let base = mock_server(vec![http_ok(SESSION_DOC)]);
    let client = JmapClient::connect(
        JmapConfig::new(
            base,
            Credentials::basic("a", "b"),
            engine_http::test_server::user_agent(),
        )
        .with_session_path("/jmap/session"),
    )
    .await
    .unwrap();
    assert!(client.session().capabilities().mail());
}

#[tokio::test]
async fn credentials_in_the_connection_url_never_reach_the_observer() {
    // `RebaseToConnection` derives both the fetched session URL and the advertised
    // `apiUrl` from the connection base, so userinfo on the base propagates into every
    // URL a step would carry. These steps exist to be logged (`north-star.md`).
    let base = mock_server(vec![http_redirect("/jmap/session"), http_ok(SESSION_DOC)]);
    let authority = base.strip_prefix("http://").unwrap();
    let recorder = Arc::new(Recorder::default());
    JmapClient::connect(
        JmapConfig::new(
            format!("http://alice:hunter2@{authority}"),
            Credentials::basic("alice", "hunter2"),
            engine_http::test_server::user_agent(),
        )
        .with_connect_observer(recorder.clone()),
    )
    .await
    .unwrap();

    let steps = recorder.steps();
    assert_eq!(
        steps,
        [
            format!("redirected {base}/.well-known/jmap -> {base}/jmap/session"),
            "authenticated".to_owned(),
            format!("discovered {base}/jmap/"),
        ]
    );
    assert!(
        !steps.concat().contains("hunter2"),
        "the password must not leak into a step: {steps:?}"
    );
}
