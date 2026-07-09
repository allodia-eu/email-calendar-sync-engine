//! Live-transport tests for [`DavClient`](super::DavClient): the real reqwest path
//! (build → request → send → collect) driven against an in-process mock HTTP server.
//!
//! The `Replay` fake the rest of the offline suite uses bypasses reqwest entirely, so
//! it can neither exercise the transport nor observe a negotiated HTTP version. Split
//! out of `transport.rs` to keep that file under the line limit.

use super::*;

/// A blocking mock HTTP server answering one canned response per connection — the
/// same shape `provider-jmap`'s tests use.
fn mock_server(responses: Vec<String>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn multistatus(body: &str) -> String {
    format!(
        "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_connected_provider_reports_the_negotiated_http_version() {
    // RFC 6764 §6 discovery is two PROPFINDs: the start URL yields the principal, the
    // principal yields the calendar-home-set.
    let base = mock_server(vec![
        multistatus(include_str!("../tests/fixtures/principal.xml")),
        multistatus(include_str!("../tests/fixtures/calendar-home.xml")),
    ]);
    let provider = crate::CalDavProvider::connect(crate::CalDavConfig::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "pw".to_owned(),
        },
    ))
    .await
    .expect("connect");

    // Discovery already exchanged a response, so the post-connect object carries the
    // version the mock server spoke.
    let info = engine_provider::Provider::connection_info(&provider);
    assert_eq!(info.http_version, Some(HttpVersion::Http1_1));
    // reqwest never exposes the negotiated TLS version, plaintext or not.
    assert_eq!(info.tls_version, None);
    assert!(info.capabilities.calendars() && info.capabilities.calendar_writes());
}

#[tokio::test]
async fn a_conditional_put_returns_its_new_etag_over_the_live_transport() {
    // The write half of the transport: a bearer-authed `PUT` carrying `If-None-Match: *`
    // (a create, RFC 4791 §5.3.2) whose `201` response names the resource's new entity
    // tag. Its response funnels through the same `collect`, so the version is recorded
    // on a write too, not only on discovery.
    let base = mock_server(vec![
        "HTTP/1.1 201 Created\r\nETag: \"v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
    ]);
    let client = DavClient::new(
        &base,
        Credentials::Bearer("tok".to_owned()),
        &engine_tls::TlsClientConfig::default(),
    )
    .expect("client");

    let response = DavExecutor::send_write(
        &client,
        WriteRequest {
            method: DavMethod::Put,
            href: "/dav/cal/default/e.ics".to_owned(),
            content_type: Some("text/calendar"),
            precondition: Precondition::IfNoneMatch,
            body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
        },
    )
    .await
    .expect("write");

    assert_eq!(
        response.into_write_etag().expect("2xx"),
        Some("\"v1\"".to_owned())
    );
    assert_eq!(
        DavExecutor::http_version(&client),
        Some(HttpVersion::Http1_1)
    );
}

#[tokio::test]
async fn a_replaying_fake_reports_no_http_version() {
    // The offline fake never speaks HTTP, so it must not invent a version — the
    // `DavExecutor` default. Guards against a provider hard-coding one.
    let fake = crate::test_support::Replay::new(Vec::new());
    assert_eq!(DavExecutor::http_version(&fake), None);
}

/// A `307` pointing at `location`.
fn redirect(location: &str) -> String {
    format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[tokio::test]
async fn connect_reports_each_hop_then_the_discovered_calendar_home() {
    // The config carries the observer, so `connect` — not some observed variant of it
    // — is what a host drives, and a redial from the same config observes for free.
    let base = mock_server(vec![
        redirect("/dav/cal"),
        multistatus(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let steps: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    let recorded = std::sync::Arc::clone(&steps);
    let provider = crate::CalDavProvider::connect(
        crate::CalDavConfig::new(
            base,
            Credentials::Basic {
                username: "alice".to_owned(),
                password: "pw".to_owned(),
            },
        )
        // The blanket `Fn` impl: a host hands over a closure, not a named type.
        .with_connect_observer(std::sync::Arc::new(
            move |step: &engine_provider::ConnectStep<'_>| {
                recorded.lock().unwrap().push(match step {
                    engine_provider::ConnectStep::Redirected { from, to, .. } => {
                        format!("redirected {from} -> {to}")
                    }
                    engine_provider::ConnectStep::Discovered { endpoint, .. } => {
                        format!("discovered {endpoint}")
                    }
                    other => format!("unexpected {other:?}"),
                });
            },
        )),
    )
    .await
    .expect("connect");

    assert_eq!(
        *steps.lock().unwrap(),
        [
            "redirected /.well-known/caldav -> /dav/cal",
            // CalDAV emits no `Authenticated` (no discrete auth exchange) and no
            // `TlsEstablished` (reqwest never exposes the negotiated version).
            "discovered /dav/cal/alice%40test.local/",
        ]
    );
    assert_eq!(
        provider.collection_href(),
        "/dav/cal/alice%40test.local/default/"
    );
}

#[tokio::test]
async fn a_connect_without_an_observer_still_discovers() {
    // Additive: the pre-existing config path is untouched.
    let base = mock_server(vec![multistatus(include_str!(
        "../tests/fixtures/principal.xml"
    ))]);
    let provider = crate::CalDavProvider::connect(crate::CalDavConfig::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "pw".to_owned(),
        },
    ))
    .await
    .expect("connect");
    assert_eq!(
        provider.collection_href(),
        "/dav/cal/alice%40test.local/default/"
    );
}

#[tokio::test]
async fn config_debug_shows_the_observer_without_leaking_the_password() {
    // `CalDavConfig`'s `Debug` is hand-written (a `dyn` observer is not `Debug`), so
    // the redaction the derive used to inherit from `Credentials` is asserted here.
    let config = crate::CalDavConfig::new(
        "https://dav.example.com",
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "hunter2".to_owned(),
        },
    );
    let shown = format!("{config:?}");
    assert!(shown.contains("alice") && shown.contains("dav.example.com"));
    assert!(
        !shown.contains("hunter2"),
        "password must not leak: {shown}"
    );
    assert!(shown.contains("connect_observer: false"), "{shown}");

    let observed = config.with_connect_observer(std::sync::Arc::new(
        |_: &engine_provider::ConnectStep<'_>| {},
    ));
    assert!(
        format!("{observed:?}").contains("connect_observer: true"),
        "an attached observer should be visible"
    );
}
