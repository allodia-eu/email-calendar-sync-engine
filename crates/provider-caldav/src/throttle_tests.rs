//! Stalwart's DAV concurrency refusal, pinned against the bytes it sent (#332).

use engine_core::error::FailureClass;
use engine_http::{RetryConfig, RetryPolicy, Throttle, ThrottleClassifier as _};

use super::*;
use crate::{
    dav_client::DavClient,
    error::CalDavError,
    test_support::mock_server_capturing,
    transport::{Credentials, DavExecutor, DavMethod, Precondition, WriteRequest},
};

/// The body of a `PUT` Stalwart v1.0.2 refused while 48 were in flight for one account.
const REFUSAL: &str = include_str!("../tests/fixtures/concurrency-limit-stalwart.json");

#[test]
fn the_refusal_is_a_throttle_to_both_verdicts() {
    // The two have to agree, because they are one verdict: the send funnel's says whether to
    // wait, the error's says what the caller is told once it stops. JMAP's #218 is what it
    // cost when only the second existed; #332 is the DAV side having neither.
    assert_eq!(
        DavThrottles.throttle(400, REFUSAL.as_bytes()),
        Some(Throttle::new())
    );
    assert_eq!(
        CalDavError::status(400, REFUSAL).failure_class(),
        FailureClass::RateLimited
    );
}

#[test]
fn every_other_400_stays_what_its_status_says() {
    // The other direction, so the reader is shown discriminating: Yahoo's empty `400`
    // (YAHOO-3), its double-encoding refusal (YAHOO-8), the request-shaped JMAP limits, and
    // bodies that are not problem details at all.
    for body in [
        "",
        "Ambiguous double encoding",
        r#"{"type":"urn:ietf:params:jmap:error:limit","limit":"maxSizeRequest"}"#,
        r#"{"type":"urn:ietf:params:jmap:error:notRequest","detail":"maxConcurrentRequests"}"#,
        "{}",
        "<html>bad request</html>",
    ] {
        assert_eq!(
            DavThrottles.throttle(400, body.as_bytes()),
            None,
            "{body:?}"
        );
        assert_eq!(
            CalDavError::status(400, body).failure_class(),
            FailureClass::Permanent,
            "{body:?}"
        );
    }
    assert_eq!(DavThrottles.throttle(400, b"\xff\xfe"), None);
}

#[test]
fn only_the_400_is_worth_reading_a_body_for() {
    // A DAV adapter reads most bodies anyway, but the funnel buffers a claimed one before it
    // decides, so a claim on `207` would hold every multistatus twice over.
    assert!(DavThrottles.reads_body_of(400));
    for settled in [200_u16, 201, 207, 401, 403, 404, 409, 412, 429, 503] {
        assert!(!DavThrottles.reads_body_of(settled), "{settled}");
    }
}

fn refused() -> String {
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/problem+json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{REFUSAL}",
        REFUSAL.len()
    )
}

fn create(
    client: &DavClient,
) -> impl Future<Output = Result<crate::transport::HttpResponse, CalDavError>> {
    DavExecutor::send_write(
        client,
        WriteRequest {
            method: DavMethod::Put,
            href: "/dav/cal/default/e.ics".to_owned(),
            content_type: Some("text/calendar"),
            precondition: Precondition::IfNoneMatch,
            body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
        },
    )
}

fn client(base: &str, retry: &RetryConfig) -> DavClient {
    DavClient::new(
        base,
        Credentials::Bearer("tok".to_owned()),
        &engine_tls::TlsClientConfig::default(),
        retry,
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

#[tokio::test]
async fn the_live_transport_waits_the_refusal_out_and_sends_the_write_again() {
    // Through the real reqwest path, because a classifier nobody wired in is a unit test of
    // nothing. Reverting `classifying` in `DavClient::new` turns this red: the refusal comes
    // back as the write's answer.
    let created =
        "HTTP/1.1 201 Created\r\nETag: \"v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, seen) = mock_server_capturing(vec![refused(), created.to_owned()]);
    let client = client(&base, &RetryConfig::default());

    let response = create(&client).await.expect("write");

    assert_eq!(
        response.into_write_etag().expect("2xx"),
        Some("\"v1\"".to_owned())
    );
    let seen = seen.lock().expect("seen");
    assert_eq!(seen.len(), 2, "sent once, refused, and sent again");
    // The replay is the same create: still guarded, so a write the refusal had somehow
    // applied would be answered `412`, never stored twice.
    assert!(
        seen[1].to_ascii_lowercase().contains("if-none-match: *"),
        "{}",
        seen[1]
    );
}

#[tokio::test]
async fn a_refusal_the_funnel_will_not_wait_for_reaches_the_caller_as_a_rate_limit() {
    // With waiting turned off, the refusal is handed straight back, and the caller is told
    // it was throttled rather than that the write is wrong.
    let (base, _) = mock_server_capturing(vec![refused()]);
    let client = client(
        &base,
        &RetryConfig::default().with_policy(RetryPolicy::none()),
    );

    let err = create(&client)
        .await
        .expect("an answer")
        .into_write_etag()
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::RateLimited);
}
