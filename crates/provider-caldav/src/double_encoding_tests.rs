//! An href whose server lists it with an octet encoded twice and then refuses it, the way Yahoo
//! lists the events it names after their `UID` (`docs/spec-deviations.md`, YAHOO-8).

use crate::{
    href::single_encoded,
    test_support::mock_server_capturing,
    transport::{Credentials, DavClient, DavExecutor, DavMethod, Precondition, WriteRequest},
};

fn client(base: &str) -> DavClient {
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

fn status(line: &str) -> String {
    format!("HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn url(path: &str) -> reqwest::Url {
    reqwest::Url::parse(&format!("https://dav.example.net{path}")).unwrap()
}

#[test]
fn a_doubly_encoded_octet_is_decoded_once() {
    assert_eq!(
        single_encoded(&url("/cal/131/uid%2540example.invalid.ics")).map(|u| u.path().to_owned()),
        Some("/cal/131/uid%40example.invalid.ics".to_owned())
    );
    assert_eq!(single_encoded(&url("/cal/131/uid%40example.ics")), None);
    assert_eq!(
        single_encoded(&url("/cal/100%25.ics")),
        None,
        "a % that encodes no octet is left alone"
    );
}

/// Each request's request line, and whether it carried `If-Match`.
fn lines(seen: &std::sync::Mutex<Vec<String>>) -> Vec<(String, bool)> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|request| {
            (
                request.lines().next().unwrap_or_default().to_owned(),
                request.to_lowercase().contains("if-match: \"e1\""),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_refused_doubly_encoded_href_is_asked_once_more_encoded_once() {
    let (base, seen) =
        mock_server_capturing(vec![status("400 Bad Request"), status("204 No Content")]);
    let client = client(&base);

    let reply = client
        .send_write(WriteRequest {
            method: DavMethod::Put,
            href: "/cal/131/uid%2540example.invalid.ics".to_owned(),
            content_type: Some("text/calendar; charset=utf-8"),
            precondition: Precondition::IfMatch("\"e1\"".to_owned()),
            body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
        })
        .await
        .expect("update");

    assert_eq!(reply.status, 204);
    assert_eq!(
        lines(&seen),
        [
            (
                "PUT /cal/131/uid%2540example.invalid.ics HTTP/1.1".to_owned(),
                true
            ),
            (
                "PUT /cal/131/uid%40example.invalid.ics HTTP/1.1".to_owned(),
                true
            ),
        ],
        "the guard goes with the second request too"
    );
}

#[tokio::test]
async fn a_400_to_an_href_with_nothing_encoded_twice_is_the_answer() {
    let (base, seen) = mock_server_capturing(vec![status("400 Bad Request")]);
    let client = client(&base);

    let reply = client
        .send(DavMethod::Report, "/cal/131/", "1", "<x/>".to_owned())
        .await
        .expect("an answer");

    assert_eq!(reply.status, 400);
    assert_eq!(seen.lock().unwrap().len(), 1, "nothing to ask again");
}

/// Without a guard nothing would stop the once-encoded URL's resource, which on a server that
/// meant the `%25` is another one, from being written.
#[tokio::test]
async fn an_unguarded_write_is_not_respelled() {
    let (base, seen) = mock_server_capturing(vec![status("400 Bad Request")]);

    let reply = client(&base)
        .send_write(WriteRequest {
            method: DavMethod::Put,
            href: "/cal/131/100%2541.ics".to_owned(),
            content_type: Some("text/calendar; charset=utf-8"),
            precondition: Precondition::None,
            body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
        })
        .await
        .expect("an answer");

    assert_eq!(reply.status, 400);
    assert_eq!(seen.lock().unwrap().len(), 1);
}
