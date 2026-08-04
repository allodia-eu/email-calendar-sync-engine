//! Live-transport tests for [`DavClient`](super::DavClient): the real reqwest path
//! (build → request → send → collect) driven against an in-process mock HTTP server.
//!
//! The `Replay` fake the rest of the offline suite uses bypasses reqwest entirely, so
//! it can neither exercise the transport nor observe a negotiated HTTP version. Split
//! out of `transport.rs` to keep that file under the line limit.

use std::sync::{Arc, Mutex};

use super::*;

/// A blocking mock HTTP server answering one canned response per connection — the
/// same shape `provider-jmap`'s tests use.
fn mock_server(responses: Vec<String>) -> String {
    mock_server_bytes(responses.into_iter().map(String::into_bytes).collect())
}

fn mock_server_bytes(responses: Vec<Vec<u8>>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(&response);
        }
    });
    format!("http://{addr}")
}

/// Serves canned responses and hands back every request's raw head, so a test can
/// assert on the headers that actually went out (notably `Authorization`).
fn mock_server_capturing(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            sink.lock()
                .expect("seen lock")
                .push(String::from_utf8_lossy(&buf[..read]).into_owned());
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

/// A vCard `PHOTO;VALUE=uri` is *card content* — it can name any host. Resolving it
/// through `Url::join` yields that foreign absolute URL unchanged, so authenticating
/// it unconditionally would hand the account's CardDAV password to whoever the card
/// names. Credentials must stay on the account's own origin.
#[tokio::test]
async fn a_foreign_photo_uri_is_fetched_without_the_account_credentials() {
    let body = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_owned();
    let (foreign, foreign_seen) = mock_server_capturing(vec![body.clone()]);
    let (base, base_seen) = mock_server_capturing(vec![body]);
    let client = DavClient::new(
        &base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "super-secret".to_owned(),
        },
        &engine_tls::TlsClientConfig::default(),
    )
    .expect("client");

    // The attacker-named host in the card gets no Authorization header at all.
    let foreign_photo = format!("{foreign}/p.png");
    DavExecutor::get_bytes(&client, &foreign_photo)
        .await
        .expect("foreign get");
    let sent = foreign_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(
        !sent.contains("authorization"),
        "credentials leaked to a foreign photo host: {sent}"
    );
    // The account's own server still authenticates as before.
    DavExecutor::get_bytes(&client, "/contacts/ada.jpg")
        .await
        .expect("same-origin get");
    let sent = base_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(
        sent.contains("authorization: basic"),
        "same-origin request lost its credentials: {sent}"
    );
}

#[tokio::test]
async fn binary_get_preserves_non_utf8_photo_bytes() {
    let photo = [0xff, 0xd8, 0xff, 0x00, 0x80, 0xd9];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        photo.len()
    )
    .into_bytes();
    response.extend_from_slice(&photo);
    let base = mock_server_bytes(vec![response]);
    let client = DavClient::new(
        &base,
        Credentials::Bearer("tok".to_owned()),
        &engine_tls::TlsClientConfig::default(),
    )
    .expect("client");

    assert_eq!(
        DavExecutor::get_bytes(&client, "/contacts/ada.jpg")
            .await
            .expect("binary get"),
        photo
    );
}

fn multistatus(body: &str) -> String {
    format!(
        "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The `OPTIONS` answer `connect` consumes after discovery: a `DAV` compliance-class
/// header and no body (RFC 4918 §10.1).
fn options_response(dav: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nDAV: {dav}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

#[tokio::test]
async fn the_scheduling_probe_is_a_bare_options_whose_dav_header_reaches_the_capability() {
    // What the replay fake structurally cannot check: it answers canned bytes whatever it
    // is sent, so only a server that reads the request can say the method is `OPTIONS`,
    // that it carries none of the read path's XML framing, and that the `DAV` header of
    // the reply is what the capability is read from.
    let (base, seen) = mock_server_capturing(vec![
        multistatus(include_str!("../tests/fixtures/principal.xml")),
        options_response(include_str!("../tests/fixtures/options-dav-stalwart.txt").trim()),
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

    assert!(
        engine_provider::Provider::connection_info(&provider)
            .capabilities
            .calendar_scheduling()
    );

    let probe = &seen.lock().expect("seen")[1];
    assert!(
        probe.starts_with("OPTIONS /dav/cal/alice%40test.local/ HTTP/1.1"),
        "the probe must be an OPTIONS on the discovered calendar home: {probe}"
    );
    let head = probe.to_lowercase();
    assert!(
        !head.contains("depth:") && !head.contains("content-type:"),
        "a bare OPTIONS must not carry the read path's Depth/XML framing: {probe}"
    );
    assert!(
        head.contains("authorization: basic"),
        "the probe authenticates like every other request: {probe}"
    );
}

#[tokio::test]
async fn a_server_advertising_no_scheduling_class_yields_a_non_scheduling_capability() {
    // SabreDAV's real header, over the real transport: `calendar-access` and no
    // `calendar-auto-schedule`. This is the account shape where a stored RSVP tells the
    // organizer nothing, so the capability must say so.
    let base = mock_server(vec![
        multistatus(include_str!("../tests/fixtures/principal.xml")),
        options_response(include_str!("../tests/fixtures/options-dav-sabredav.txt").trim()),
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
    let caps = engine_provider::Provider::connection_info(&provider).capabilities;
    assert!(!caps.calendar_scheduling());
    // …and the rest of the calendar capability is unaffected: this server reads, writes
    // and can express an answer. Only the delivery promise is missing.
    assert!(caps.calendars() && caps.calendar_writes() && caps.calendar_rsvp().is_some());
}

#[tokio::test]
async fn a_connected_provider_reports_the_negotiated_http_version() {
    // RFC 6764 §6 discovery is two PROPFINDs: the start URL yields the principal, the
    // principal yields the calendar-home-set.
    let base = mock_server(vec![
        multistatus(include_str!("../tests/fixtures/principal.xml")),
        options_response("1, 3, calendar-access"),
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
        options_response("1, 3, calendar-access"),
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
    let base = mock_server(vec![
        multistatus(include_str!("../tests/fixtures/principal.xml")),
        options_response("1, 3, calendar-access"),
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
