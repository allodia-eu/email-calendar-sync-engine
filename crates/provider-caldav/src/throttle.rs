//! The one DAV refusal whose status does not say it is one.
//!
//! Stalwart counts a DAV request against the same per-account cap as a JMAP one
//! (`maxConcurrentRequests`, 4 on the harness), and refuses a request over it the way JMAP
//! refuses an API call: a `400` carrying RFC 8620 §3.6.1's problem details,
//! `urn:ietf:params:jmap:error:limit` with `limit: "maxConcurrentRequests"`. Read by its
//! status that is a request defect, so before this a `PUT` that would have succeeded a moment
//! later was reported `Permanent` (#332). RFC 9110 §15.5.1 lets a `400` stand for any error the
//! server perceives as the client's, so this is permitted rather than a departure
//! (`docs/spec-deviations.md`, "Permitted, and easy to get wrong").
//!
//! The same body `provider-jmap` reads in `JmapThrottles`. It is recognised again here rather
//! than shared because the two adapters depend on nothing in common that could own it:
//! `engine-http` is deliberately ignorant of every provider's error vocabulary
//! (`ThrottleClassifier`'s own docs say why), and this is JMAP's vocabulary turning up on a
//! DAV endpoint, not DAV's.

/// Whether `body` is the problem details of a refusal a client clears by waiting.
///
/// `maxConcurrentRequests` alone: of RFC 8620's three `limit` values it is the only one that
/// describes the server's load rather than the request, and the other two cannot reach a DAV
/// endpoint anyway. Parsed, not substring-matched, so a body merely quoting the name is not one.
pub(crate) fn is_concurrency_limit(body: &str) -> bool {
    let Ok(problem) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    problem.get("type").and_then(serde_json::Value::as_str)
        == Some("urn:ietf:params:jmap:error:limit")
        && problem.get("limit").and_then(serde_json::Value::as_str) == Some("maxConcurrentRequests")
}

/// Reads the concurrency refusal out of the `400` that carries it, so the shared send funnel
/// waits it out instead of handing it back.
///
/// **Safe to replay whatever the method**, which is what answering `Some` asserts: the cap is
/// checked before the request runs. Measured on the harness (v1.0.2): of 48 simultaneous
/// create-only `PUT`s, 34 were refused, and a `GET` of each refused resource afterwards
/// answered `404` — none was stored.
///
/// No wait is ever named. The refusal carries `RateLimit: "concurrent-requests";r=0` and no
/// reset, and a slot frees when another request finishes, which the engine's own backoff
/// schedule is the right guess for.
///
/// There is no gate narrowing to go with it, unlike JMAP's: a DAV server states no ceiling in
/// the protocol, and SabreDAV has none to state (`http-throttling.md`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DavThrottles;

impl engine_http::ThrottleClassifier for DavThrottles {
    fn reads_body_of(&self, status: u16) -> bool {
        // Only the `400`. Every other `400` a DAV server sends — Yahoo's empty one to a report
        // it lacks (YAHOO-3), its "Ambiguous double encoding" (YAHOO-8) — is read here, found
        // not to be this, and handed back as it was.
        status == 400
    }

    fn throttle(&self, _status: u16, body: &[u8]) -> Option<engine_http::Throttle> {
        let body = core::str::from_utf8(body).ok()?;
        is_concurrency_limit(body).then(engine_http::Throttle::new)
    }
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
