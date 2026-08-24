// SPDX-License-Identifier: MPL-2.0
//! Typed EAS status enums + the single source-of-truth mapping from a status
//! code to a `RecoveryAction`. Pure / no I/O — callers in `client.rs` and
//! `eas_source.rs` consult this instead of open-coding recovery decisions.

/// The set of actions a caller can take in response to an EAS status code.
/// Ordered roughly from "no-op" to "fatal". Every classifier in this module
/// returns one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Apply the returned delta / response normally.
    Ok,
    /// Run the Provision handshake, then retry the original command once.
    /// Triggered by Common 142/143/144, FolderSync 142, HTTP 449.
    RetryProvision,
    /// Reset the collection sync_key to "0" and re-bootstrap the folder
    /// cache. Triggered by Sync 3, FolderSync 9.
    ResetSyncKey,
    /// Run FolderSync (hierarchy changed). Triggered by Sync 12, Ping 7.
    /// MVP note: the engine's cache-wipe path is a safe superset, so this
    /// currently degrades to `ResetSyncKey` at the call site.
    RunFolderSync,
    /// Refresh the OAuth access token, then retry once. HTTP 401 + OAuth only.
    RefreshToken,
    /// Follow the X-MS-Location URL and retry. HTTP 451 only. The transport
    /// layer adopts the location's endpoint and re-issues the command,
    /// hop-capped (`client::MAX_REDIRECT_HOPS`).
    FollowRedirect,
    /// Wait then retry at the engine layer (60s poll). HTTP 429 / 5xx.
    RetryTransient,
    /// Surface an "authentication required" error to the user — token
    /// refresh failed, or Basic auth rejected. HTTP 401 + Basic, 403.
    SurfaceAuth,
    /// Surface a non-recoverable error (do NOT silently retry). Includes
    /// Common 140 (remote wipe) and 126, unknown statuses, malformed responses.
    SurfacePermanent,
}

/// Recovery decision for a top-level Common status (MS-AS* top-level Status).
pub fn recovery_action_for_common(status: u32) -> RecoveryAction {
    match status {
        1 => RecoveryAction::Ok,
        142..=144 => RecoveryAction::RetryProvision,
        140 | 126 => RecoveryAction::SurfacePermanent,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for a Sync collection status (MS-ASSYNC 2.2.3.23).
pub fn recovery_action_for_sync(status: u32) -> RecoveryAction {
    match status {
        1 | 6 => RecoveryAction::Ok,
        3 => RecoveryAction::ResetSyncKey,
        12 => RecoveryAction::RunFolderSync,
        // MS-ASCMD 2.2.3.177.17: 5 = "Server error … frequently a transient
        // condition … retry"; 16 = "Retry — retriable error, resend the
        // request". Both are folder-scoped transient retries, NOT permanent
        // failures (Android treats 5/16 as backoff-retry).
        5 | 16 => RecoveryAction::RetryTransient,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for a FolderSync status (MS-ASFolderSync 2.2.3.1.10).
pub fn recovery_action_for_folder_sync(status: u32) -> RecoveryAction {
    match status {
        1 => RecoveryAction::Ok,
        9 => RecoveryAction::ResetSyncKey,
        142 => RecoveryAction::RetryProvision,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for a Ping status (MS-ASPing 2.2.3.7).
pub fn recovery_action_for_ping(status: u32) -> RecoveryAction {
    match status {
        // MS-ASCMD 2.2.3.177.11: 1 = heartbeat expired with NO changes (the
        // normal idle expiry), 2 = changes found in one or more folders.
        // (The old comment had these inverted; the arm itself was always
        // correct — both are healthy outcomes, handled at the engine layer.)
        1 | 2 => RecoveryAction::Ok,
        7 => RecoveryAction::RunFolderSync,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for a Provision status (MS-ASPROV 2.2.3.x).
pub fn recovery_action_for_provision(status: u32) -> RecoveryAction {
    match status {
        1 => RecoveryAction::Ok,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for a ComposeMail / SendMail status
/// (MS-ASCMD §2.2.3.90 SendMail, §2.2.3.162 Status codes).
///
/// NOTE: the SendMail-specific status code mapping is PROVISIONAL pending
/// validation against a real Exchange server in manual end-to-end testing. The
/// structure + recovery semantics are what matter now; refine the match
/// arms once we see real `<Status>` values from SendMail responses.
///
/// The classifier is defined so the 3b retry layer / ComposeMail status
/// handling can call it for SendMail responses later. For T5 it is not
/// wired into `send_command` — SendMail success is an empty body, errors
/// come back as `EasError` variants which `map_eas_error` translates. If a
/// SendMail `<Status>` error surfaces as `EasError::CommandStatus`, that is
/// where this classifier would map it.
///
/// Mapping (provisional, modeled on the Common family):
///   * 140/141/142/143/144 — Provisioning family → `RetryProvision` (same recovery as Common
///     142-144 / FolderSync 142). The retry layer in `send_command` already runs Provision on HTTP
///     449; these status codes are the in-body equivalent for SendMail.
///   * 111 / 132 — transient (server temporarily unavailable / retry) → `RetryTransient` (engine's
///     60s poll loop is the retry).
///   * 130 / 131 — fatal-auth (authentication required / credentials rejected) → `SurfaceAuth`.
///   * anything else — `SurfacePermanent` (do NOT retry unknown codes blindly; surface to the user
///     / breaker).
pub fn recovery_action_for_send_mail(status: u32) -> RecoveryAction {
    match status {
        140..=144 => RecoveryAction::RetryProvision,
        111 | 132 => RecoveryAction::RetryTransient,
        130 | 131 => RecoveryAction::SurfaceAuth,
        _ => RecoveryAction::SurfacePermanent,
    }
}

/// Recovery decision for an HTTP status. The OAuth-vs-Basic distinction
/// matters for 401: OAuth → try refresh; Basic → surface immediately.
pub fn recovery_action_for_http(status: u16, is_oauth: bool) -> RecoveryAction {
    match status {
        200 => RecoveryAction::Ok,
        401 if is_oauth => RecoveryAction::RefreshToken,
        401 | 403 => RecoveryAction::SurfaceAuth,
        429 => RecoveryAction::RetryTransient,
        449 => RecoveryAction::RetryProvision,
        451 => RecoveryAction::FollowRedirect,
        500..=599 => RecoveryAction::RetryTransient,
        _ => RecoveryAction::SurfacePermanent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_142_143_144_retry_provision() {
        assert_eq!(
            recovery_action_for_common(142),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_common(143),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_common(144),
            RecoveryAction::RetryProvision
        );
    }

    #[test]
    fn common_140_126_surface_permanent() {
        assert_eq!(
            recovery_action_for_common(140),
            RecoveryAction::SurfacePermanent
        );
        assert_eq!(
            recovery_action_for_common(126),
            RecoveryAction::SurfacePermanent
        );
    }

    #[test]
    fn common_1_ok() {
        assert_eq!(recovery_action_for_common(1), RecoveryAction::Ok);
    }

    #[test]
    fn sync_3_reset_sync_key_12_run_folder_sync() {
        assert_eq!(recovery_action_for_sync(3), RecoveryAction::ResetSyncKey);
        assert_eq!(recovery_action_for_sync(12), RecoveryAction::RunFolderSync);
        assert_eq!(recovery_action_for_sync(1), RecoveryAction::Ok);
        assert_eq!(recovery_action_for_sync(6), RecoveryAction::Ok);
        // MS-ASCMD 2.2.3.177.17 — Sync 5 ("Server error … frequently a
        // transient condition … retry") and 16 ("Retry — retriable error,
        // resend the request") are TRANSIENT: the engine retries next round
        // without escalating the per-folder failure backoff (previously both
        // were pinned permanent, which parked busy folders like Deleted
        // Items). Android's client treats 5/16 the same way.
        assert_eq!(recovery_action_for_sync(5), RecoveryAction::RetryTransient);
        assert_eq!(recovery_action_for_sync(16), RecoveryAction::RetryTransient);
        // Sync 4/8 remain permanent (protocol error, object not found) —
        // exhaustive pin so a future refactor can't silently regress these
        // to a retry path.
        assert_eq!(
            recovery_action_for_sync(4),
            RecoveryAction::SurfacePermanent
        );
        assert_eq!(
            recovery_action_for_sync(8),
            RecoveryAction::SurfacePermanent
        );
    }

    #[test]
    fn folder_sync_9_reset_142_provision_126_permanent() {
        assert_eq!(
            recovery_action_for_folder_sync(9),
            RecoveryAction::ResetSyncKey
        );
        assert_eq!(
            recovery_action_for_folder_sync(142),
            RecoveryAction::RetryProvision
        );
        // MS-ASCMD: 126 = userDisabledForSync — permanent, NOT provision.
        assert_eq!(
            recovery_action_for_folder_sync(126),
            RecoveryAction::SurfacePermanent
        );
        assert_eq!(recovery_action_for_folder_sync(1), RecoveryAction::Ok);
    }

    #[test]
    fn ping_7_run_folder_sync() {
        assert_eq!(recovery_action_for_ping(7), RecoveryAction::RunFolderSync);
        assert_eq!(recovery_action_for_ping(1), RecoveryAction::Ok);
        assert_eq!(recovery_action_for_ping(2), RecoveryAction::Ok);
        // Plan mapping table — Ping 5 = bad parameters, surface permanently.
        assert_eq!(
            recovery_action_for_ping(5),
            RecoveryAction::SurfacePermanent
        );
    }

    /// Provision status contract. The mapping pins 1→Ok, 2/3→SurfacePermanent.
    /// Earlier iterations skipped provision tests entirely; this is the
    /// exhaustive pin for the Provision column.
    #[test]
    fn provision_status_table_is_exhaustive() {
        assert_eq!(recovery_action_for_provision(1), RecoveryAction::Ok);
        assert_eq!(
            recovery_action_for_provision(2),
            RecoveryAction::SurfacePermanent
        );
        assert_eq!(
            recovery_action_for_provision(3),
            RecoveryAction::SurfacePermanent
        );
        // Any other status is also permanent (defensive default).
        assert_eq!(
            recovery_action_for_provision(999),
            RecoveryAction::SurfacePermanent
        );
    }

    #[test]
    fn http_401_oauth_refresh_basic_surface_auth() {
        assert_eq!(
            recovery_action_for_http(401, true),
            RecoveryAction::RefreshToken
        );
        assert_eq!(
            recovery_action_for_http(401, false),
            RecoveryAction::SurfaceAuth
        );
        assert_eq!(
            recovery_action_for_http(403, true),
            RecoveryAction::SurfaceAuth
        );
        assert_eq!(
            recovery_action_for_http(429, true),
            RecoveryAction::RetryTransient
        );
        assert_eq!(
            recovery_action_for_http(449, true),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_http(451, true),
            RecoveryAction::FollowRedirect
        );
        assert_eq!(
            recovery_action_for_http(503, true),
            RecoveryAction::RetryTransient
        );
        assert_eq!(recovery_action_for_http(200, true), RecoveryAction::Ok);
    }

    /// SendMail status classifier. The mapping is PROVISIONAL pending
    /// real-Exchange manual end-to-end validation; these assertions pin the
    /// recovery semantics so a future refinement of the match arms can't silently
    /// regress them. Variant names are aligned to the real `RecoveryAction` enum
    /// (not the temporary pseudocode aliases used in design notes).
    #[test]
    fn send_mail_status_maps_provisioning_retry_auth_fatal() {
        // Provisioning family (140/141/142/143/144) → RetryProvision.
        assert_eq!(
            recovery_action_for_send_mail(140),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_send_mail(141),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_send_mail(142),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_send_mail(143),
            RecoveryAction::RetryProvision
        );
        assert_eq!(
            recovery_action_for_send_mail(144),
            RecoveryAction::RetryProvision
        );
        // Transient (111/132) → RetryTransient.
        assert_eq!(
            recovery_action_for_send_mail(111),
            RecoveryAction::RetryTransient
        );
        assert_eq!(
            recovery_action_for_send_mail(132),
            RecoveryAction::RetryTransient
        );
        // Fatal auth (130/131) → SurfaceAuth.
        assert_eq!(
            recovery_action_for_send_mail(130),
            RecoveryAction::SurfaceAuth
        );
        assert_eq!(
            recovery_action_for_send_mail(131),
            RecoveryAction::SurfaceAuth
        );
        // Unknown codes must NOT retry blindly → SurfacePermanent.
        assert_eq!(
            recovery_action_for_send_mail(1),
            RecoveryAction::SurfacePermanent
        );
        assert_eq!(
            recovery_action_for_send_mail(999),
            RecoveryAction::SurfacePermanent
        );
    }
}
