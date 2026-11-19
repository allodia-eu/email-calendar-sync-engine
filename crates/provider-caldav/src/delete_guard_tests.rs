//! A guarded delete against a server that would perform it whatever the guard said, the way
//! Yahoo does (`docs/spec-deviations.md`, YAHOO-9).

use crate::{
    test_support::{mock_server_capturing, multistatus},
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
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

fn etag(value: &str) -> String {
    multistatus(&format!(
        r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>{value}</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    ))
}

fn status(line: &str) -> String {
    format!("HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn delete(precondition: Precondition) -> WriteRequest {
    WriteRequest {
        method: DavMethod::Delete,
        href: "/cal/a.ics".to_owned(),
        content_type: None,
        precondition,
        body: String::new(),
    }
}

fn methods(seen: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|request| request.split(' ').next().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn a_delete_from_a_superseded_copy_is_refused_without_being_sent() {
    let (base, seen) = mock_server_capturing(vec![etag("\"e2\"")]);

    let reply = client(&base)
        .send_write(delete(Precondition::IfMatch("\"e1\"".to_owned())))
        .await
        .expect("an answer");

    assert_eq!(reply.status, 412, "the conflict a host refetches on");
    assert_eq!(methods(&seen), ["PROPFIND"], "no DELETE was sent");
}

#[tokio::test]
async fn a_delete_from_the_current_copy_is_sent_with_its_guard() {
    // The listing writes the ETag without its quotes; the guard still matches.
    let (base, seen) = mock_server_capturing(vec![etag("e1"), status("204 No Content")]);

    let reply = client(&base)
        .send_write(delete(Precondition::IfMatch("\"e1\"".to_owned())))
        .await
        .expect("an answer");

    assert_eq!(reply.status, 204);
    assert_eq!(methods(&seen), ["PROPFIND", "DELETE"]);
    assert!(
        seen.lock().unwrap()[1]
            .to_lowercase()
            .contains("if-match: \"e1\""),
        "the server still gets the guard"
    );
}

#[tokio::test]
async fn a_delete_goes_ahead_when_the_etag_cannot_be_read() {
    let (base, seen) = mock_server_capturing(vec![
        status("405 Method Not Allowed"),
        status("204 No Content"),
    ]);

    let reply = client(&base)
        .send_write(delete(Precondition::IfMatch("\"e1\"".to_owned())))
        .await
        .expect("an answer");

    assert_eq!(reply.status, 204);
    assert_eq!(methods(&seen), ["PROPFIND", "DELETE"]);
}

#[tokio::test]
async fn an_unguarded_delete_reads_nothing_first() {
    let (base, seen) = mock_server_capturing(vec![status("204 No Content")]);

    client(&base)
        .send_write(delete(Precondition::None))
        .await
        .expect("an answer");

    assert_eq!(methods(&seen), ["DELETE"]);
}

#[tokio::test]
async fn a_delete_guarded_by_a_superseded_schedule_tag_is_refused_without_being_sent() {
    let (base, seen) = mock_server_capturing(vec![multistatus(
        r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><c:schedule-tag>"s2"</c:schedule-tag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
    )]);

    let reply = client(&base)
        .send_write(delete(Precondition::IfScheduleTagMatch(
            "\"s1\"".to_owned(),
        )))
        .await
        .expect("an answer");

    assert_eq!(reply.status, 412);
    assert_eq!(methods(&seen), ["PROPFIND"], "no DELETE was sent");
}
