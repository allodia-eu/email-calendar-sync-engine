// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

// ---- Phase 3b Task 5: send_command retry layer ----
//
// The `send_command_http_retry` layer classifies an HTTP-level error returned
// by `send_command_no_retry` and decides whether a retry is warranted. This
// enum is the output of the pure decision function below — kept separate from
// the more granular `crate::status::RecoveryAction` because the transport
// layer only acts on three recovery types (provision, refresh, redirect);
// everything else (Ok, RateLimited, SurfaceAuth, SurfacePermanent) is surfaced
// to the caller unchanged.

/// Hop cap for the HTTP 451 `X-MS-Location` redirect follow per command call
/// ([MS-ASHTTP] §3.1.5.2). Matches autodiscover's `MAX_REDIRECTS`. Exceeding
/// the cap surfaces the last 451 — a server redirect cycle must never loop
/// forever.
pub(super) const MAX_REDIRECT_HOPS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryDecision {
    /// Do not retry — surface the original error. Covers HTTP 401+Basic,
    /// 403, 429, 5xx (the engine's 60s poll loop is the retry for those),
    /// and any other non-recoverable status.
    None,
    /// Run the Provision handshake (`self.provision()`), then re-issue the
    /// command once. Triggered by HTTP 449 (Provision required).
    RunProvision,
    /// Refresh the OAuth access token (`auth.refresh()`), then re-issue once.
    /// Triggered by HTTP 401 when the account is on the OAuth path.
    RefreshToken,
    /// Follow the `X-MS-Location` redirect: adopt the new endpoint URL and
    /// re-issue the command. Triggered by HTTP 451 ([MS-ASHTTP] §3.1.5.2).
    /// The retry layer hop-caps the follow at [`MAX_REDIRECT_HOPS`] per
    /// command call, then surfaces the 451.
    FollowRedirect,
}

/// Classify an HTTP status into a retry decision for the
/// `send_command_http_retry` layer. Pure / no I/O — delegates to
/// `crate::status::recovery_action_for_http` and maps the granular
/// `RecoveryAction` into the three actions the transport can take itself.
///
/// `is_oauth` distinguishes 401-on-OAuth (refresh) from 401-on-Basic (surface)
/// — mirrors the `recovery_action_for_http` contract.
pub(super) fn retry_decision_for_http_err(status: u16, is_oauth: bool) -> RetryDecision {
    use crate::status::RecoveryAction as A;
    match crate::status::recovery_action_for_http(status, is_oauth) {
        A::RetryProvision => RetryDecision::RunProvision,
        A::RefreshToken => RetryDecision::RefreshToken,
        A::FollowRedirect => RetryDecision::FollowRedirect,
        // Ok: the wrapper is only called on Err(HttpStatus), so Ok never
        // arrives here in practice; treat it as None defensively.
        // RetryTransient (429/5xx), SurfaceAuth (401 Basic / 403),
        // ResetSyncKey/RunFolderSync (command-level, not HTTP), and
        // SurfacePermanent all surface — the engine's poll loop / breaker
        // handles retries for those.
        _ => RetryDecision::None,
    }
}

/// Per-hop decision for the HTTP 451 redirect follow loop ([MS-ASHTTP]
/// §3.1.5.2) — the pure boundary `send_command_http_retry` consults each
/// time `send_command_no_retry` returns a 451:
///   * hops below [`MAX_REDIRECT_HOPS`] with a location → [`RedirectHop::Follow`] (the location is
///     handed back for adoption; the caller bumps the hop counter and re-issues);
///   * hops at/above the cap → [`RedirectHop::HopCapReached`] (surface the 451 — a server redirect
///     cycle must never loop forever);
///   * no location → [`RedirectHop::NoLocation`] (surface — cannot follow what the server didn't
///     tell us).
///
/// Pure / no I/O — the loop wiring needs a live server, but this boundary
/// is unit-tested without one.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RedirectHop<'a> {
    /// Adopt this location as the new endpoint and re-issue the command.
    Follow(&'a str),
    /// Hop cap reached — surface the 451 instead of following further.
    HopCapReached,
    /// No X-MS-Location to follow — surface the 451.
    NoLocation,
}

pub(super) fn redirect_hop_decision(hops: u32, location: Option<&str>) -> RedirectHop<'_> {
    if hops >= MAX_REDIRECT_HOPS {
        RedirectHop::HopCapReached
    } else {
        match location {
            Some(l) => RedirectHop::Follow(l),
            None => RedirectHop::NoLocation,
        }
    }
}

/// Parse a `Retry-After` header value (delta-seconds form) into an absolute
/// epoch-seconds timestamp. Returns `None` for:
///   - the HTTP-date form (RFC 7231 §7.1.3, e.g. `Wed, 21 Oct 2026 07:28:00 GMT`) — we deliberately
///     do NOT parse it; the caller falls back to the default rate-limit window. This is an honest,
///     documented limitation (HTTP-date support is tracked as a follow-up).
///   - any non-integer / unparseable input.
///
/// Pure / no I/O — extracted from the HTTP-response handling path so it is
/// unit-testable without a live socket. The caller passes the current epoch
/// (`SystemTime::now()` at the response site) so the parser itself is
/// deterministic.
pub(super) fn parse_retry_after_delta(header_value: &str, now_epoch: i64) -> Option<i64> {
    let delta: i64 = header_value.trim().parse().ok()?;
    Some(now_epoch + delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Phase 3f Task 5: Retry-After (delta-seconds) parsing ----

    #[test]
    fn parse_retry_after_delta_seconds() {
        // Plain delta-seconds: result is now + delta.
        assert_eq!(parse_retry_after_delta("30", 1000), Some(1030));
        // Whitespace is tolerated (reqwest/HeaderValue strips most already, but
        // `trim()` makes the parser robust to a stray leading/trailing space).
        assert_eq!(parse_retry_after_delta("  120  ", 0), Some(120));
        // HTTP-date form (RFC 7231 §7.1.3) — we do NOT parse it; caller falls
        // back to the default window. Honest limitation.
        assert_eq!(
            parse_retry_after_delta("Wed, 21 Oct 2026 07:28:00 GMT", 0),
            None
        );
        // Non-numeric / empty -> None.
        assert_eq!(parse_retry_after_delta("garbage", 0), None);
        assert_eq!(parse_retry_after_delta("", 0), None);
    }

    // ---- Phase 3b Task 5: send_command retry DECISION ----
    //
    // `retry_decision_for_http_err` is the pure decision function the
    // `send_command_http_retry` layer consults after `send_command_no_retry`
    // returns an `EasError::HttpStatus`. The wrapper itself needs a live
    // server (covered by Task 7's manual e2e); the decision logic is
    // unit-testable.

    #[test]
    fn retry_decision_449_triggers_provision() {
        let d = retry_decision_for_http_err(449, false);
        assert_eq!(d, RetryDecision::RunProvision);
    }

    #[test]
    fn retry_decision_401_oauth_triggers_refresh() {
        let d = retry_decision_for_http_err(401, true);
        assert_eq!(d, RetryDecision::RefreshToken);
    }

    #[test]
    fn retry_decision_401_basic_no_retry() {
        let d = retry_decision_for_http_err(401, false);
        assert_eq!(d, RetryDecision::None);
    }

    #[test]
    fn retry_decision_451_triggers_redirect() {
        let d = retry_decision_for_http_err(451, false);
        assert_eq!(d, RetryDecision::FollowRedirect);
    }
    #[test]
    fn redirect_hop_follows_below_cap_when_location_present() {
        for hops in 0..MAX_REDIRECT_HOPS {
            match redirect_hop_decision(
                hops,
                Some("https://new.example.com/Microsoft-Server-ActiveSync"),
            ) {
                RedirectHop::Follow(location) => assert_eq!(
                    location, "https://new.example.com/Microsoft-Server-ActiveSync",
                    "hops={hops} (< cap) must hand back the location unchanged"
                ),
                other => panic!("hops={hops} (< cap) with a location must Follow, got {other:?}"),
            }
        }
    }

    #[test]
    fn redirect_hop_surfaces_at_hop_cap_boundary() {
        // Boundary: hops == cap surfaces even WITH a valid location.
        assert_eq!(
            redirect_hop_decision(
                MAX_REDIRECT_HOPS,
                Some("https://new.example.com/Microsoft-Server-ActiveSync")
            ),
            RedirectHop::HopCapReached
        );
        // The decision stays closed beyond the cap (the loop cannot legally
        // get there, but the boundary must hold regardless).
        assert_eq!(
            redirect_hop_decision(
                MAX_REDIRECT_HOPS + 1,
                Some("https://new.example.com/Microsoft-Server-ActiveSync")
            ),
            RedirectHop::HopCapReached
        );
    }

    #[test]
    fn redirect_hop_surfaces_without_location_below_cap() {
        assert_eq!(redirect_hop_decision(0, None), RedirectHop::NoLocation);
        assert_eq!(
            redirect_hop_decision(MAX_REDIRECT_HOPS - 1, None),
            RedirectHop::NoLocation
        );
    }
}
