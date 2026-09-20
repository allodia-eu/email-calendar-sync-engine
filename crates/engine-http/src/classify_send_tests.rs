//! What [`send_retrying`](crate::send_retrying) does with a reply whose **status does not
//! classify it** — the half of the decision an adapter owns.
//!
//! The bodies here are reduced to the one field each classifier keys on; the real bytes are
//! pinned in the adapters, against captures from Gmail and Stalwart. What is under test is
//! the funnel: when a body is read, what happens to the reply afterwards, and that a
//! classifier can only ever add a throttle and never take one away.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use crate::{
    send_retrying,
    send_test_support::{Reply, client, recording, scripted},
};

/// Gmail's quota refusal, reduced to the two things the adapter reads: the status is `403`
/// and the body says `rateLimitExceeded`. The real bytes are pinned in `provider-google`
/// against a captured fixture; what is being tested here is the funnel, not the JSON.
const QUOTA: &str = r#"{"error":{"code":403,"errors":[{"reason":"rateLimitExceeded"}]}}"#;
/// A `403` that is exactly what it says: no permission, and no amount of waiting fixes it.
const FORBIDDEN: &str = r#"{"error":{"code":403,"errors":[{"reason":"insufficientPermissions"}]}}"#;

/// The statuses an adapter like `provider-google` claims. Deliberately not `429`: that one
/// the status rule already settles, and claiming it would buy nothing but a buffered body.
const READS: &[u16] = &[403];

/// A classifier shaped like a real adapter's: a `403` is a throttle when its body says so.
fn quota_classifier() -> Arc<dyn crate::ThrottleClassifier> {
    Arc::new((READS, |_status: u16, body: &[u8]| {
        std::str::from_utf8(body)
            .is_ok_and(|text| text.contains("rateLimitExceeded"))
            .then(crate::Throttle::new)
    }))
}

#[tokio::test(start_paused = true)]
async fn a_403_the_adapter_recognises_as_a_quota_refusal_is_waited_out() {
    // The bug #218 reports, in one test: before classification this returned the `403` on
    // the first send, the scope's sync failed, and the work waited for the next pass.
    let (url, served) = scripted(vec![
        Reply("403 Forbidden", "", QUOTA),
        Reply("200 OK", "", ""),
    ]);
    let (retry, log) = recording();
    let retry = retry.classifying(quota_classifier());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200, "the caller sees success");
    assert_eq!(served.load(Ordering::SeqCst), 2, "it sent again");
    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1, "and told the host why it paused");
    assert_eq!(events[0].0, 403, "reported as the status that arrived");
}

#[tokio::test(start_paused = true)]
async fn a_403_that_really_is_a_permission_failure_is_handed_straight_back_with_its_body() {
    // The other half of the classifier's job, and the more important one: retrying a real
    // `403` spends a quota unit to be told the same thing. The body must survive being
    // read — the adapter's own error carries it, and a reply put back together empty would
    // turn every permission failure into an unexplained one.
    let (url, served) = scripted(vec![Reply("403 Forbidden", "", FORBIDDEN)]);
    let (retry, log) = recording();
    let retry = retry.classifying(quota_classifier());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(served.load(Ordering::SeqCst), 1, "not retried");
    assert!(
        log.lock().unwrap().is_empty(),
        "and not reported as a throttle"
    );
    assert_eq!(response.text().await.expect("body"), FORBIDDEN);
}

#[tokio::test(start_paused = true)]
async fn a_status_the_adapter_did_not_claim_is_never_read_at_all() {
    // `reads_body_of` is the cost control: a reply outside the claimed statuses is returned
    // exactly as it arrived, unbuffered and unrebuilt.
    let (url, served) = scripted(vec![Reply("404 Not Found", "", QUOTA)]);
    let (retry, _) = recording();
    let retry = retry.classifying(quota_classifier());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 404);
    assert_eq!(served.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_classifier_can_add_a_throttle_and_never_take_one_away() {
    // The additive property the funnel depends on. This classifier claims `429` and denies
    // every body it is shown; the `429` must still be waited out, because the status rule
    // is applied first and on its own.
    const CLAIMS_EVERYTHING: &[u16] = &[403, 429, 503];
    let (url, served) = scripted(vec![
        Reply("429 Too Many Requests", "", "not a throttle, honest"),
        Reply("200 OK", "", ""),
    ]);
    let (retry, _) = recording();
    let retry = retry.classifying(Arc::new((
        CLAIMS_EVERYTHING,
        |_status: u16, _body: &[u8]| None,
    )));
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "the 429 was still waited out"
    );
}

#[tokio::test(start_paused = true)]
async fn a_wait_the_adapter_read_out_of_the_body_is_treated_as_the_servers_own() {
    // Gmail names no `Retry-After`, but its refusal carries the quota window's start, so
    // the wait to the window's end is a number the server stated. It is then treated
    // exactly as a header would be — including by the bound on what gets absorbed. A
    // window with three seconds left is a hiccup, and the funnel sleeps on it.
    let (url, served) = scripted(vec![
        Reply("403 Forbidden", "", QUOTA),
        Reply("200 OK", "", ""),
    ]);
    let (retry, log) = recording();
    let retry = retry.classifying(Arc::new((READS, |_status: u16, _body: &[u8]| {
        Some(crate::Throttle::after(Duration::from_secs(3)))
    })));
    let started = tokio::time::Instant::now();
    send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(served.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "backoff would have guessed 250ms, inside a window that had not reset",
    );
    let (_, _, delay, named, _) = log.lock().unwrap()[0];
    assert_eq!(
        named,
        Some(Duration::from_secs(3)),
        "the host is told the figure, not merely that there was one",
    );
    assert!(delay >= Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn a_quota_window_that_has_just_begun_is_reported_rather_than_slept_on() {
    // The other end of the same number, and the case that matters most in practice: trip
    // the quota early in its window and the reset is most of a minute away. Sleeping there
    // parks the task and holds the account's gate lane against a quota that is one
    // provider's, so the instant goes back to the caller to schedule.
    let (url, served) = scripted(vec![Reply("403 Forbidden", "", QUOTA)]);
    let (retry, log) = recording();
    let retry = retry.classifying(Arc::new((READS, |_status: u16, _body: &[u8]| {
        Some(crate::Throttle::after(Duration::from_secs(50)))
    })));
    let started = tokio::time::Instant::now();
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(served.load(Ordering::SeqCst), 1, "it did not sleep on it");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        response.stated_wait(),
        Some(Duration::from_secs(50)),
        "a wait read out of a body reaches the caller exactly as a header's would",
    );
    assert!(log.lock().unwrap()[0].4, "reported as a give-up");
    assert_eq!(response.text().await.expect("body"), QUOTA);
}

#[tokio::test(start_paused = true)]
async fn a_reply_put_back_together_keeps_everything_the_adapter_reads_off_it() {
    // Reading the body takes the reply apart. An adapter reads a status, headers and bytes
    // off what comes back, so all three have to cross over — an `ETag` dropped here is a
    // write that cannot be guarded.
    let (url, _) = scripted(vec![Reply(
        "403 Forbidden",
        "ETag: \"abc123\"\r\nContent-Type: application/json\r\n",
        FORBIDDEN,
    )]);
    let (retry, _) = recording();
    let retry = retry.classifying(quota_classifier());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(response.headers()["etag"], "\"abc123\"");
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(
        response.headers()["content-length"],
        FORBIDDEN.len().to_string(),
        "restated for the body that is actually attached",
    );
    assert_eq!(response.version(), reqwest::Version::HTTP_11);
    assert_eq!(response.text().await.expect("body"), FORBIDDEN);
}

#[tokio::test(start_paused = true)]
async fn a_classified_throttle_that_never_clears_is_handed_back_with_its_body_intact() {
    // The give-up path, which is the one that reconstructs a reply nobody asked to keep.
    // The adapter still turns it into its own error, and that error carries the body.
    let (url, served) = scripted(vec![Reply("403 Forbidden", "", QUOTA)]);
    let (retry, log) = recording();
    let retry = retry.classifying(quota_classifier());
    let response = send_retrying(client().get(&url), &retry)
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(served.load(Ordering::SeqCst), 5, "the default attempt cap");
    assert!(log.lock().unwrap()[4].4, "reported as a give-up");
    assert_eq!(response.text().await.expect("body"), QUOTA);
}
