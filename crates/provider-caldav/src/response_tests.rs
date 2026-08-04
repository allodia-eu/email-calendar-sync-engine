//! Unit tests for [`HttpResponse`](super::HttpResponse) — the pure response→outcome
//! mapping (redirect detection, status classification, the write `ETag`) with no HTTP in
//! sight. The reqwest path is `transport_tests.rs`; this is a sibling file so
//! `transport.rs` stays under the line limit.

use super::*;

fn response(status: u16, location: Option<&str>) -> HttpResponse {
    HttpResponse {
        status,
        body: String::new(),
        location: location.map(str::to_owned),
        etag: None,
        dav: None,
    }
}

#[test]
fn redirect_detection_requires_a_location() {
    assert!(response(307, Some("/dav/cal")).is_redirect());
    assert!(!response(307, None).is_redirect());
    // 303 See Other is a redirect too (must be followed by discovery).
    assert!(response(303, Some("/dav/cal")).is_redirect());
}

#[test]
fn non_207_status_becomes_a_classified_error() {
    let unauthorized = HttpResponse {
        status: 401,
        body: "denied".to_owned(),
        location: None,
        etag: None,
        dav: None,
    };
    let err = unauthorized.into_multistatus().unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Authentication
    );
}

#[test]
fn dav_method_tokens() {
    assert_eq!(DavMethod::Propfind.as_str(), "PROPFIND");
    assert_eq!(DavMethod::Get.as_str(), "GET");
    assert_eq!(DavMethod::Report.as_str(), "REPORT");
    assert_eq!(DavMethod::Put.as_str(), "PUT");
    assert_eq!(DavMethod::Delete.as_str(), "DELETE");
}

#[test]
fn write_success_yields_the_new_etag() {
    // A 2xx PUT returns the server's new entity tag (or None when it sent none).
    let created = HttpResponse {
        status: 201,
        body: String::new(),
        location: None,
        etag: Some("\"v9\"".to_owned()),
        dav: None,
    };
    assert_eq!(
        created.into_write_etag().unwrap(),
        Some("\"v9\"".to_owned())
    );
    let no_content = HttpResponse {
        status: 204,
        body: String::new(),
        location: None,
        etag: None,
        dav: None,
    };
    assert_eq!(no_content.into_write_etag().unwrap(), None);
}

#[test]
fn write_precondition_failure_is_a_conflict() {
    // RFC 4791 §5.3.2: a failed If-Match/If-None-Match is 412 → Conflict, so
    // the caller refetches rather than blindly retrying.
    let precondition_failed = HttpResponse {
        status: 412,
        body: String::new(),
        location: None,
        etag: None,
        dav: None,
    };
    let err = precondition_failed.into_write_etag().unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Conflict
    );
}
