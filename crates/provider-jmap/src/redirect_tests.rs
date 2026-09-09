//! Session discovery across a redirect chain that changes origin.
//!
//! The shape these lock is the one RFC 8620 §2.2 invites and every same-origin test
//! misses: a provider whose apex sends the client to a different host, which then
//! answers with a bare path of its own. Both hops are ordinary; together they broke
//! discovery outright, because the `Location` was resolved against the configured
//! base rather than against the URL that issued it, and the session's advertised
//! `apiUrl` was then rebased onto a host that never served the session.
//!
//! Two mock servers, so "which origin did this land on" is an assertion and not an
//! assumption. `lib_tests.rs` keeps the single-origin chain.

use std::sync::{Arc, Mutex};

use engine_provider::{ConnectObserver, ConnectStep};

use super::*;

/// A single-shot mock serving `http_responses` in order, one per connection.
fn mock_server(http_responses: Vec<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for response in http_responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_redirect(status: &str, location: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// Advertises an absolute `apiUrl` on a public hostname the test never connects to —
/// the Stalwart shape `SessionUrlPolicy::RebaseToConnection` exists to correct.
const SESSION_DOC: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{"maxObjectsInGet":500},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"https://mail.test.local/jmap/"}"#;

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl ConnectObserver for Recorder {
    fn step(&self, step: &ConnectStep) {
        let line = match step {
            ConnectStep::Redirected { from, to, .. } => format!("redirected {from} -> {to}"),
            ConnectStep::Authenticated => "authenticated".to_owned(),
            ConnectStep::Discovered { endpoint, .. } => format!("discovered {endpoint}"),
            other => format!("{other:?}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

impl Recorder {
    fn steps(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// The reported failure, reproduced: the apex redirects to a second host, which serves
/// the session from a path of its own. Before the fix this exhausted the hop budget and
/// failed as `too many session redirects` — the `Location` was rebased back onto the
/// connection origin, so hop one resolved to the URL it came from and the chain
/// oscillated in place.
#[tokio::test]
async fn connect_follows_a_well_known_redirect_to_another_origin() {
    let mail = mock_server(vec![
        http_redirect("307 Temporary Redirect", "/jmap/session"),
        http_ok(SESSION_DOC),
    ]);
    let apex = mock_server(vec![http_redirect(
        "302 Found",
        &format!("{mail}/.well-known/jmap"),
    )]);

    let client = JmapClient::connect(JmapConfig::new(
        apex.clone(),
        Credentials::basic("alice@example.com", "pw"),
    ))
    .await
    .unwrap();

    assert!(client.session().capabilities().mail());
    // The whole point: every method call must go to the host that actually served the
    // session, not to the apex the account was configured against.
    assert_eq!(client.session().api_url(), format!("{mail}/jmap/"));
}

/// The second half of the same chain, isolated: a bare-path `Location` arriving *after*
/// an origin change belongs to the new origin. Resolved against the configured base it
/// would name a path on the apex, which is a different server.
#[tokio::test]
async fn a_relative_redirect_after_an_origin_change_stays_on_the_new_origin() {
    let mail = mock_server(vec![
        http_redirect("307 Temporary Redirect", "/jmap/session"),
        http_ok(SESSION_DOC),
    ]);
    let apex = mock_server(vec![http_redirect(
        "302 Found",
        &format!("{mail}/.well-known/jmap"),
    )]);

    let recorder = Arc::new(Recorder::default());
    JmapClient::connect(
        JmapConfig::new(apex.clone(), Credentials::basic("a", "b"))
            .with_connect_observer(recorder.clone()),
    )
    .await
    .unwrap();

    assert_eq!(
        recorder.steps(),
        [
            format!("redirected {apex}/.well-known/jmap -> {mail}/.well-known/jmap"),
            format!("redirected {mail}/.well-known/jmap -> {mail}/jmap/session"),
            "authenticated".to_owned(),
            format!("discovered {mail}/jmap/"),
        ]
    );
}

/// A chain that genuinely loops still terminates. Removing the rebase removed what was
/// accidentally bounding this, so the hop budget is now the only thing that does.
#[tokio::test]
async fn a_redirect_loop_still_fails_rather_than_spinning() {
    let looping = (0..=MAX_SESSION_REDIRECTS)
        .map(|_| http_redirect("307 Temporary Redirect", "/.well-known/jmap"))
        .collect();
    let base = mock_server(looping);

    let err = JmapClient::connect(JmapConfig::new(base, Credentials::basic("a", "b")))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("too many session redirects"),
        "unexpected error: {err}"
    );
}

/// A `Location` the client cannot resolve is an error, not a silently re-requested URL.
#[tokio::test]
async fn an_unresolvable_redirect_location_is_reported() {
    let base = mock_server(vec![http_redirect("302 Found", "http://[::bad")]);
    let err = JmapClient::connect(JmapConfig::new(base, Credentials::basic("a", "b")))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unresolvable redirect"),
        "unexpected error: {err}"
    );
}
