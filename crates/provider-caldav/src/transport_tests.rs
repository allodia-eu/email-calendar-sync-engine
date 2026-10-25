//! Live-transport tests for [`DavClient`](super::DavClient): the real reqwest path
//! (build → request → send → collect) driven against an in-process mock HTTP server.
//!
//! The `Replay` fake the rest of the offline suite uses bypasses reqwest entirely, so
//! it can neither exercise the transport nor observe a negotiated HTTP version. Split
//! out of `transport.rs` to keep that file under the line limit.

use super::*;
use crate::test_support::{
    mock_server, mock_server_bytes, mock_server_capturing, multistatus, options_response, redirect,
};

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
        &engine_http::RetryConfig::default(),
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
        &engine_http::RetryConfig::default(),
    )
    .expect("client");

    assert_eq!(
        DavExecutor::get_bytes(&client, "/contacts/ada.jpg")
            .await
            .expect("binary get"),
        photo
    );
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
    // The mock server speaks plaintext, so there is no TLS version on that response.
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
        &engine_http::RetryConfig::default(),
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
            base.clone(),
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
            // Each href absolute: discovery resolves what it reads against the request that
            // returned it (RFC 4918 §8.3).
            format!("redirected {base}/.well-known/caldav -> {base}/dav/cal"),
            // CalDAV emits no `Authenticated` (no discrete auth exchange) and no
            // `TlsEstablished` (an HTTP adapter learns its TLS version from a response,
            // after the connect phase).
            format!("discovered {base}/dav/cal/alice%40test.local/"),
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

/// A challenge in the credential's scheme, then an answer: what a server sends a request that
/// arrives without credentials and then one that carries them.
fn challenge() -> String {
    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"dav\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        .to_owned()
}

fn ok_body() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_owned()
}

fn basic_client(base: &str) -> DavClient {
    DavClient::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "super-secret".to_owned(),
        },
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

/// The counterpart of the foreign-photo rule: an origin the account's own server led
/// discovery to may receive the credential, but only once it asks for it (RFC 9110 §11.5:
/// a protection space does not extend past its own server), and from then on unasked.
#[tokio::test]
async fn a_trusted_origin_is_sent_the_credential_only_once_it_asks() {
    let (moved, moved_seen) = mock_server_capturing(vec![challenge(), ok_body(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);

    assert!(client.trust_origin(&format!("{moved}/principals/u/")));
    let photo = format!("{moved}/calendars/u/a.png");
    DavExecutor::get_bytes(&client, &photo)
        .await
        .expect("answered after the challenge");
    DavExecutor::get_bytes(&client, &photo)
        .await
        .expect("answered again");

    let seen = moved_seen.lock().expect("seen");
    let carried: Vec<bool> = seen
        .iter()
        .map(|request| request.to_lowercase().contains("authorization: basic"))
        .collect();
    assert_eq!(
        carried,
        [false, true, true],
        "unasked, answered, then sent: {seen:?}"
    );
}

/// A foreign host that *asks* for the credential still does not get it: only an origin the
/// account's server led discovery to may answer a challenge.
#[tokio::test]
async fn a_foreign_photo_host_that_asks_for_the_credential_is_not_sent_it() {
    let (foreign, foreign_seen) = mock_server_capturing(vec![challenge(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);

    let err = DavExecutor::get_bytes(&client, &format!("{foreign}/p.png"))
        .await
        .expect_err("the challenge stands");
    assert!(err.to_string().contains("401"), "{err}");
    let seen = foreign_seen.lock().expect("seen");
    assert_eq!(seen.len(), 1, "asked once, never again with the password");
    assert!(!seen[0].to_lowercase().contains("authorization"));
}

/// Settling on the home moves where relative hrefs resolve: every href a response from the
/// home carries is relative to the home's origin (RFC 4918 §8.3).
#[tokio::test]
async fn settling_on_a_home_resolves_relative_hrefs_onto_its_origin() {
    let (moved, moved_seen) = mock_server_capturing(vec![challenge(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);
    let home = format!("{moved}/calendars/u/");

    assert!(client.trust_origin(&home));
    assert_eq!(client.settle_on(&home), "/calendars/u/");
    DavExecutor::get_bytes(&client, "/calendars/u/a.png")
        .await
        .expect("get after settling");

    let sent = moved_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(sent.contains("get /calendars/u/a.png "), "{sent}");
    assert!(sent.contains("authorization: basic"), "{sent}");
}

/// Only a resolvable, origin-bearing URL names anything to trust or settle on: a href a
/// server merely mentions moves nothing.
#[tokio::test]
async fn a_url_that_names_no_origin_trusts_and_moves_nothing() {
    let (base, _) = mock_server_capturing(vec![]);
    let client = DavClient::new(
        &base,
        Credentials::Bearer("t".to_owned()),
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    let before = format!("{client:?}");
    for noise in ["/principals/u/", "not a url", "data:text/plain,hi", ""] {
        assert!(client.trust_origin(noise), "refused {noise:?}");
        assert_eq!(client.settle_on(noise), noise);
        assert_eq!(format!("{client:?}"), before, "moved by {noise:?}");
    }
}

/// An origin the server leads discovery to is trusted only while it stays on TLS.
/// `href::redirect_href` cannot catch this hop, since discovery starts at a bare well-known
/// path that names no scheme, so the connection is what refuses, and the walk fails rather
/// than putting the account's password on the wire in the clear.
#[tokio::test]
async fn an_origin_that_leaves_tls_is_not_trusted() {
    let client = basic_client("https://dav.example.com");
    assert!(!client.trust_origin("http://evil.test/principals/u/"));
    assert!(client.trust_origin("https://dav.example.net/principals/u/"));
}
