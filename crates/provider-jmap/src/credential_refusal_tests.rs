//! A `401` stays a refusal however its body ends: a submission refused is never "may have been
//! sent", and a `/set` refused never "may have been applied" (both read the status, which is
//! the server's own answer).

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_provider::{Credential, HandOver, Unrecorded};
use serde_json::json;

use super::{credential_tests::Rotating, transport_tests::mock_server};

/// A `401` that promises a 100-byte body, sends ten bytes, and closes.
const CUT_OFF_REFUSAL: &str = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"jmap\"\r\nContent-Length: 100\r\nConnection: close\r\n\r\n0123456789";

#[tokio::test]
async fn a_refusal_cut_off_mid_body_leaves_a_submission_refused_not_ambiguous() {
    let (base, seen) = mock_server(vec![CUT_OFF_REFUSAL.to_owned()]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "wrong"),
        Vec::new(),
    ));
    let transport = super::credential_tests::transport(&source);
    let hand_over = HandOver::new(&Unrecorded);
    let envelope = json!({"using": [], "methodCalls": []});
    let Err(err) = transport
        .post_json(
            &base,
            &envelope,
            engine_http::Exchange::Submission(&hand_over),
        )
        .await
    else {
        panic!("refused")
    };
    assert_eq!(seen.lock().unwrap().len(), 1);
    // The server's own answer, so a `/set` refused so was not applied either.
    assert!(
        matches!(err, crate::JmapError::Status { status: 401, .. }),
        "{err:?}"
    );
    let as_submission = err.into_submission_error();
    assert_eq!(
        as_submission.class(),
        FailureClass::Authentication,
        "{as_submission}"
    );
}
