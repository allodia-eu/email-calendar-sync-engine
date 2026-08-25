// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// EAS HTTP client. Wraps `reqwest` to send WBXML POST requests to an Exchange
// ActiveSync endpoint and parse WBXML responses. Each command (FolderSync,
// Sync, SendMail, etc.) has its own high-level method that delegates to the
// pure marshalers in `commands.rs`.

use base64::Engine;

use crate::{
    commands,
    types::*,
    wbxml::{WbxmlElement, WbxmlError, deserialize_to_tree, serialize_tree},
};

const PAGE_FOLDER: u8 = 7;
const PAGE_COMPOSE: u8 = 21;
const PAGE_ITEM_OPS: u8 = 20;
const PAGE_PING: u8 = 13;
const PAGE_AIRSYNC: u8 = 0;

/// AirSync (page 0) `Sync` root token. Used by the Sync response `expect_root`
/// check so a non-Sync response (server error page, OWA redirect, etc.) is
/// surfaced as `UnexpectedRoot` rather than a confusing parse failure.
const AS_SYNC: u8 = 0x05;

const FH_FOLDER_SYNC: u8 = 0x16;
const FH_FOLDER_CREATE: u8 = 0x13;
const FH_FOLDER_DELETE: u8 = 0x14;
const FH_FOLDER_UPDATE: u8 = 0x15;

const CM_SEND_MAIL: u8 = 0x05;
const CM_SMART_FORWARD: u8 = 0x06;
const CM_SMART_REPLY: u8 = 0x07;

const IO_ITEMOPERATIONS: u8 = 0x05;
const PING_PING: u8 = 0x05;

const GIE_ROOT: u8 = 0x05;

const PAGE_SEARCH: u8 = 15;
const SR_SEARCH: u8 = 0x05;

/// Move (page 5) root token for MoveItems — [MS-ASWBXML] §2.1.2.1.6.
const PAGE_MOVE: u8 = 5;
const MV_MOVE_ITEMS: u8 = 0x05;

/// MeetingResponse (page 8) root token — [MS-ASWBXML] §2.1.2.1.9.
const PAGE_MREQ: u8 = 8;
const MREQ_MEETING_RESPONSE: u8 = 0x07;

// ---- Command size limits ([MS-ASCMD] §3.1.5.10 "Limiting Size of Command
// Requests") ----
//
// Clients SHOULD limit the number of elements per command request; servers
// SHOULD enforce the tabulated limits and return the documented error status
// when exceeded (Exchange 2010 SP2 UR6 and later do — §3.1.5.10 product
// notes <27>/<28>). We chunk client-side so we never trip a server-side
// rejection. ItemOperations (≤100 ops), MeetingResponse (≤100 Requests), and
// GetItemEstimate (≤1000 collections) need no chunking here: their builders
// emit exactly one unit per call (see the single-op verification in the
// task-5 report).

/// [MS-ASCMD] §3.1.5.10: a MoveItems request SHOULD carry at most 1000 Move
/// elements (spec minimum 1). A server enforcing the limit answers MoveItems
/// Status 4 (§2.2.3.177.10) — `move_items` splits larger batches into
/// sequential ≤1000-move commands instead of risking that rejection.
const MOVE_ITEMS_MAX_PER_COMMAND: usize = 1000;

/// [MS-ASCMD] §3.1.5.10: the sum of the Add + Change + Delete + Fetch
/// elements in a Sync request SHOULD be at most 200 (spec minimum 1). The
/// upsync only emits Change elements (`build_sync_change_request`), so the
/// change count IS the element count; `sync_changes` splits larger batches
/// into sequential ≤200-change commands, threading the rotated sync key.
const SYNC_MAX_COMMANDS_PER_REQUEST: usize = 200;

/// Error returned by any EAS operation. Combines transport, WBXML, and
/// protocol-level errors (status codes).
#[derive(Debug, thiserror::Error)]
pub enum EasError {
    #[error("HTTP transport error: {0}")]
    Transport(String),
    /// Non-200 HTTP response. `retry_after` carries the parsed `Retry-After`
    /// header (delta-seconds form, converted to absolute epoch) when the server
    /// sent one alongside a 429/503; `None` otherwise (including the HTTP-date
    /// form, which we do not parse — caller falls back to a default window).
    /// The EAS source promotes a 429/503 HttpStatus with `retry_after` to
    /// `SourceError::RateLimited`.
    /// `x_ms_location` carries the `X-MS-Location` response header on HTTP
    /// 451 ([MS-ASHTTP] §2.2.1.1.2.4) — the full URL of the server the
    /// mailbox moved to. The retry layer adopts it (hop-capped) and retries;
    /// `None` for every other status and for a 451 that omits the header.
    #[error("HTTP {status}: {body}")]
    HttpStatus {
        status: u16,
        body: String,
        retry_after: Option<i64>,
        x_ms_location: Option<String>,
    },
    #[error("WBXML codec error: {0}")]
    Wbxml(#[from] WbxmlError),
    #[error("unexpected response root: page {page} token {token}")]
    UnexpectedRoot { page: u8, token: u8 },
    #[error("command status {status}: {message}")]
    CommandStatus { status: u32, message: String },
    /// Client-side request validation failure — the request was rejected
    /// before ANY network I/O (distinct from `CommandStatus`, which means
    /// the SERVER actively rejected a request that went out).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Authentication failure: the OAuth token provider could not produce an
    /// access token (IdP/network failure, or the refresh grant is dead and the
    /// user must re-authenticate). Surfaces to the caller so it can prompt the
    /// user to re-authenticate. Basic auth never produces this error —
    /// `EasAuth::refresh()` is a no-op for Basic. (Absorbs the temporary
    /// `auth::AuthError`; kylins' old `AuthRefreshFailed` maps here.)
    #[error("authentication failed: {0}")]
    Auth(String),
}

impl From<reqwest::Error> for EasError {
    fn from(e: reqwest::Error) -> Self {
        EasError::Transport(e.to_string())
    }
}

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
const MAX_REDIRECT_HOPS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecision {
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
fn retry_decision_for_http_err(status: u16, is_oauth: bool) -> RetryDecision {
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
enum RedirectHop<'a> {
    /// Adopt this location as the new endpoint and re-issue the command.
    Follow(&'a str),
    /// Hop cap reached — surface the 451 instead of following further.
    HopCapReached,
    /// No X-MS-Location to follow — surface the 451.
    NoLocation,
}

fn redirect_hop_decision<'a>(hops: u32, location: Option<&'a str>) -> RedirectHop<'a> {
    if hops >= MAX_REDIRECT_HOPS {
        RedirectHop::HopCapReached
    } else {
        match location {
            Some(l) => RedirectHop::Follow(l),
            None => RedirectHop::NoLocation,
        }
    }
}

/// Decide whether a failed SmartForward should degrade to plain SendMail.
/// True only for `EasError::CommandStatus` — the server actively rejected
/// the SmartForward semantics (e.g. original attachments gone, source item
/// not found), so any status qualifies. Transport / HTTP / WBXML errors are
/// transient or local and the SmartForward may still succeed on retry — the
/// caller surfaces those unchanged.
fn should_degrade_to_send_mail(err: &EasError) -> bool {
    matches!(err, EasError::CommandStatus { .. })
}

/// Decide whether a SmartForward in-body `<Status>` (HTTP 200 path, surfaced
/// by `parse_send_mail_response` as `Ok(status)`) counts as a rejection that
/// should degrade to plain SendMail. EAS compose rejections commonly arrive
/// this way rather than as an Err, so any status other than success (1)
/// qualifies.
fn smart_forward_inbody_status_failed(status: u32) -> bool {
    status != 1
}

/// Decide what an empty response body means for this command.
/// `None` → caller treats it as success (SendMail family per MS-ASCMD);
/// `Some(Err)` → surface the transport error.
fn empty_body_outcome(cmd_allowed: bool) -> Option<EasError> {
    if cmd_allowed {
        None
    } else {
        Some(EasError::Transport("empty response body".into()))
    }
}

/// Build the no-changes SyncResult for an empty Sync response body.
/// Exchange returns an EMPTY HTTP body for Sync when the collection has
/// nothing to report (Android EasSync.java:225 treats empty as OK), so this
/// is a success result — NOT an error. CRITICAL: the sync key is the
/// REQUEST's key, not Default's empty string — an empty key would corrupt
/// the engine's persisted cursor and force a full re-sync. SyncResult's
/// manual Default already yields status 1 (success) and empty item lists.
fn no_changes_result(request_key: &str) -> SyncResult {
    SyncResult {
        sync_key: request_key.to_string(),
        ..Default::default()
    }
}

/// Split a batch of per-element command units into slices no larger than the
/// [MS-ASCMD] §3.1.5.10 SHOULD-limit for one wire command. The chunks cover
/// the input contiguously and in order (backing the callers' result-merge
/// ordering contract). Empty input yields zero chunks — the callers then
/// send nothing. Pure / no I/O: the unit-testable boundary behind the
/// `move_items` / `sync_changes` chunked send loops (same split as
/// `redirect_hop_decision`).
fn command_chunks<T>(items: &[T], max_per_command: usize) -> Vec<&[T]> {
    items.chunks(max_per_command).collect()
}

/// High-level EAS client. Cheap to clone (just wraps a `reqwest::Client` and config).
#[derive(Clone)]
pub struct EasClient {
    config: EasConfig,
    http: reqwest::Client,
    /// Folder-hierarchy sync key from the last successful FolderSync.
    /// Folder ops (Create/Update/Delete) must send it per MS-ASCMD; empty
    /// until the first FolderSync, then `hierarchy_key()` yields "0".
    hierarchy_sync_key: String,
    /// The last HTTP 451 `X-MS-Location` redirect target adopted during a
    /// command round ([MS-ASHTTP] §3.1.5.2). `None` until a redirect is
    /// adopted; `EasSource::persist_eas_url_if_changed` reads it after a
    /// successful round to persist the adopted endpoint into
    /// `accounts.eas_url`, mirroring the policy-key persistence.
    adopted_url: Option<String>,
}

impl EasClient {
    pub fn new(config: EasConfig) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .danger_accept_invalid_certs(config.accept_invalid_certs);
        // User-Agent
        let ua = config.user_agent.clone();
        if !ua.is_empty() {
            builder = builder.user_agent(&ua);
        }
        let http = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            http,
            hierarchy_sync_key: String::new(),
            adopted_url: None,
        }
    }

    /// The hierarchy sync key for folder ops — "0" before the first FolderSync.
    pub fn hierarchy_key(&self) -> &str {
        if self.hierarchy_sync_key.is_empty() {
            "0"
        } else {
            &self.hierarchy_sync_key
        }
    }

    /// Read-only access to the in-memory cached hierarchy sync key ("0"
    /// before the first successful FolderSync, via the same fallback as
    /// `hierarchy_key()`). `EasSource::list_folders` persists this after a
    /// successful FolderSync round so the next round resumes with the
    /// server-issued key instead of re-bootstrapping from "0".
    pub fn hierarchy_sync_key_str(&self) -> &str {
        self.hierarchy_key()
    }

    /// Prime the in-memory hierarchy sync key from the persisted one
    /// (`accounts.eas_hierarchy_key`) so a folder op (FolderCreate/Update/
    /// Delete) can go out WITHOUT a preceding FolderSync round — the op
    /// requests carry the key per MS-ASCMD, and a fresh client would
    /// otherwise send the bootstrap key "0" (stale-key status in reply).
    /// Cheaper than issuing a FolderSync just to warm the cache. An empty
    /// string is ignored (same "0" fallback as `hierarchy_key()`).
    pub fn set_hierarchy_sync_key(&mut self, key: String) {
        if !key.is_empty() {
            self.hierarchy_sync_key = key;
        }
    }

    /// Read-only access to the current policy key. The retry layer's
    /// `RunProvision` branch rotates this in place via `provision()`; the
    /// source layer reads it after a successful round to persist the rotated
    /// key for the next sync. Avoids leaking the full `EasConfig` (which
    /// carries secrets).
    pub fn policy_key(&self) -> &str {
        &self.config.policy_key
    }

    /// Read-only access to the redirect endpoint adopted during this
    /// client's command rounds (HTTP 451 `X-MS-Location` follow), or `None`
    /// when no redirect was adopted. The source layer reads it after a
    /// successful round to persist the adopted URL (`accounts.eas_url`),
    /// mirroring the `policy_key()` persistence. Avoids leaking the full
    /// `EasConfig` (which carries secrets).
    pub fn adopted_url(&self) -> Option<&str> {
        self.adopted_url.as_deref()
    }

    /// Adopt an HTTP 451 `X-MS-Location` redirect target ([MS-ASHTTP]
    /// §2.2.1.1.2.4 / §3.1.5.2): validate the location via
    /// [`endpoint_from_x_ms_location`], switch this client's base URL to the
    /// derived endpoint, and record the adopted URL for the source layer to
    /// persist. Logs the hop (from → to) at info. An invalid location is
    /// logged at warn and surfaced as an error — the old URL stays untouched
    /// and nothing is recorded.
    ///
    /// `pub` (not `pub(crate)`) because the host's source layer exercises it
    /// directly in redirect-persistence tests (kylins'
    /// `sync::eas_source::persist_eas_url_writes_adopted_url_against_current_row`).
    pub fn adopt_redirect_location(&mut self, location: &str) -> Result<(), EasError> {
        let new_url = endpoint_from_x_ms_location(location).map_err(|e| {
            log::warn!("EAS HTTP 451 redirect not followed: {e}");
            e
        })?;
        let old_url = std::mem::replace(&mut self.config.url, new_url.clone());
        self.adopted_url = Some(new_url.clone());
        log::info!(
            "EAS HTTP 451 redirect hop: {old_url} → {new_url} — retrying against the new server"
        );
        Ok(())
    }

    /// Public entry: send an EAS command with classified retries.
    ///
    /// Two retry layers:
    /// 1. HTTP-level (`send_command_http_retry`): 449 → Provision (one retry), 401 OAuth → token
    ///    refresh (one retry), 451 → follow the `X-MS-Location` redirect (adopt the new endpoint
    ///    and re-issue, hop-capped at `MAX_REDIRECT_HOPS`).
    /// 2. In-body top-level Status (this wrapper): Common 142–144 (device not provisioned / policy
    ///    refresh / invalid policy key) → run the two-phase Provision handshake once, then re-issue
    ///    the command once. The retried response is returned as-is.
    ///
    /// `provision()` sends via `send_command_no_retry`, so neither layer can
    /// recurse. Command-specific statuses (Sync 3, FolderSync 9, …) are NOT
    /// interpreted here — callers map them via `status::recovery_action_for_*`
    /// as before.
    ///
    /// `allow_empty` controls what an HTTP 200 with an empty body means:
    /// `true` (SendMail family per MS-ASCMD) → `Ok(None)`; `false` → error.
    ///
    /// `timeout` is an optional per-request timeout override (Ping passes
    /// heartbeat + margin; reqwest's `RequestBuilder::timeout` overrides the
    /// client-wide 120s default). `None` keeps the client default.
    pub async fn send_command_ex(
        &mut self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        allow_empty: bool,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<WbxmlElement>, EasError> {
        self.send_command_ex_opts(cmd_name, request_root, allow_empty, timeout, false)
            .await
    }

    /// `send_command_ex` plus the multipart opt-in ([MS-ASCMD] §2.2.1.10.1):
    /// when `accept_multipart` is true the POST carries
    /// `MS-ASAcceptMultiPart: T` and a `application/vnd.ms-sync.multipart`
    /// response is parsed (parts resolved into inline base64 Data) instead
    /// of rejected. Only `item_operations` passes true — it is the only
    /// command the spec defines multipart responses for.
    async fn send_command_ex_opts(
        &mut self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        allow_empty: bool,
        timeout: Option<std::time::Duration>,
        accept_multipart: bool,
    ) -> Result<Option<WbxmlElement>, EasError> {
        let root = self
            .send_command_http_retry(
                cmd_name,
                request_root,
                allow_empty,
                timeout,
                accept_multipart,
            )
            .await?;
        // The in-body top-level-status provision retry only inspects
        // `Some(tree)` — `None` (empty-body success) passes through unchanged.
        if let Some(ref tree) = root
            && let Some(status) = commands::top_level_status(tree)
            && crate::status::recovery_action_for_common(status)
                == crate::status::RecoveryAction::RetryProvision
        {
            log::info!(
                "EAS {cmd_name} returned status {status} — running Provision and retrying once"
            );
            self.provision().await?;
            return self
                .send_command_no_retry(
                    cmd_name,
                    request_root,
                    allow_empty,
                    timeout,
                    accept_multipart,
                )
                .await;
        }
        Ok(root)
    }

    /// Public entry: send an EAS command with classified retries.
    /// Empty bodies are always errors on this path — commands that treat an
    /// empty body as success (SendMail family) use `send_command_ex`.
    pub async fn send_command(
        &mut self,
        cmd_name: &str,
        request_root: &WbxmlElement,
    ) -> Result<WbxmlElement, EasError> {
        self.send_command_timed(cmd_name, request_root, None).await
    }

    /// Like `send_command`, but applies a per-request timeout override that
    /// wins over the client's global 120s default. Used by Ping (heartbeat +
    /// margin); every other command goes through `send_command` (`None`).
    pub async fn send_command_timed(
        &mut self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        timeout: Option<std::time::Duration>,
    ) -> Result<WbxmlElement, EasError> {
        self.send_command_ex(cmd_name, request_root, false, timeout)
            .await?
            .ok_or_else(|| EasError::Transport("empty response body".into()))
    }

    /// HTTP-level retry layer: send an EAS command, applying classified
    /// retries on transport-level signals (HTTP 449 Provision required, HTTP
    /// 401 OAuth token refresh, HTTP 451 redirect).
    ///
    /// This layer only acts on `EasError::HttpStatus` because that is the only
    /// error class where a blind retry (without consulting the parsed
    /// response) is correct. In-body top-level Status provision retries are
    /// handled one level up by the public `send_command` wrapper; nested
    /// command-specific statuses (Sync 3, FolderSync 9, …) are decoded by the
    /// caller via `status::recovery_action_for_*`.
    ///
    /// `RunProvision` runs the full two-phase Provision handshake and
    /// `RefreshToken` rotates the OAuth access token — each retries the
    /// command ONCE (no loops). `FollowRedirect` ([MS-ASHTTP] §3.1.5.2)
    /// adopts the response's `X-MS-Location` endpoint — switching
    /// `self.config.url` in place — and re-issues the command, up to
    /// [`MAX_REDIRECT_HOPS`] hops per call; beyond the cap (or with a
    /// missing/invalid location) the 451 surfaces, so a redirect cycle can
    /// never loop forever. Provision, OAuth refresh, and redirect follow all
    /// mutate `self.config`, so this method takes `&mut self`.
    async fn send_command_http_retry(
        &mut self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        allow_empty: bool,
        timeout: Option<std::time::Duration>,
        accept_multipart: bool,
    ) -> Result<Option<WbxmlElement>, EasError> {
        let mut redirect_hops = 0u32;
        loop {
            match self
                .send_command_no_retry(
                    cmd_name,
                    request_root,
                    allow_empty,
                    timeout,
                    accept_multipart,
                )
                .await
            {
                Ok(root) => return Ok(root),
                Err(EasError::HttpStatus {
                    status,
                    body,
                    retry_after,
                    x_ms_location,
                }) => {
                    let is_oauth = self
                        .config
                        .auth
                        .as_ref()
                        .map(|a| a.is_oauth())
                        .unwrap_or(false);
                    match retry_decision_for_http_err(status, is_oauth) {
                        RetryDecision::RunProvision => {
                            // Re-provision, then retry the original command once.
                            // provision() itself calls send_command_no_retry
                            // internally (never the retry wrapper) so there is no
                            // unbounded recursion.
                            self.provision().await?;
                            return self
                                .send_command_no_retry(
                                    cmd_name,
                                    request_root,
                                    allow_empty,
                                    timeout,
                                    accept_multipart,
                                )
                                .await;
                        }
                        RetryDecision::RefreshToken => {
                            // OAuth only — Basic EasAuth::refresh() is a no-op.
                            if let Some(auth) = self.config.auth.as_mut() {
                                auth.refresh().await?;
                            }
                            return self
                                .send_command_no_retry(
                                    cmd_name,
                                    request_root,
                                    allow_empty,
                                    timeout,
                                    accept_multipart,
                                )
                                .await;
                        }
                        RetryDecision::FollowRedirect => {
                            // [MS-ASHTTP] §2.2.1.1.2.4 / §3.1.5.2: adopt the
                            // X-MS-Location endpoint and re-issue the command
                            // against it. `redirect_hop_decision` is the pure
                            // hop boundary (unit-tested); adoption and the
                            // warn logs live here. Hop-capped so a server
                            // redirect cycle surfaces instead of looping
                            // forever.
                            let hop =
                                redirect_hop_decision(redirect_hops, x_ms_location.as_deref());
                            match hop {
                                RedirectHop::Follow(location) => {
                                    // Validates the location, switches the base
                                    // URL, records adopted_url, logs the hop.
                                    // An invalid location surfaces as an
                                    // error (old URL untouched).
                                    self.adopt_redirect_location(location)?;
                                    redirect_hops += 1;
                                }
                                RedirectHop::HopCapReached => {
                                    log::warn!(
                                        "EAS {cmd_name}: HTTP 451 after {redirect_hops} redirect hops — hop cap ({MAX_REDIRECT_HOPS}) reached; surfacing the 451"
                                    );
                                    return Err(EasError::HttpStatus {
                                        status,
                                        body,
                                        retry_after,
                                        x_ms_location,
                                    });
                                }
                                RedirectHop::NoLocation => {
                                    log::warn!(
                                        "EAS {cmd_name}: HTTP 451 without an X-MS-Location header — cannot follow; surfacing the 451"
                                    );
                                    return Err(EasError::HttpStatus {
                                        status,
                                        body,
                                        retry_after,
                                        x_ms_location,
                                    });
                                }
                            }
                        }
                        RetryDecision::None => {
                            // Surface the original error intact — body and
                            // Retry-After preserved so the source layer's
                            // rate-limit promotion sees the server's window.
                            return Err(EasError::HttpStatus {
                                status,
                                body,
                                retry_after,
                                x_ms_location,
                            });
                        }
                    }
                }
                // Transport / WBXML / CommandStatus errors: surface, don't retry.
                // The engine's 60s poll loop is the retry for transient failures.
                Err(e) => return Err(e),
            }
        }
    }

    /// Single EAS command request, no retry. Sends WBXML bytes, reads WBXML
    /// response, deserializes to a tree. The public `send_command` wraps this
    /// (via `send_command_http_retry`) with the classified retry layers;
    /// `provision()` calls this directly so its internal command sends never
    /// recurse through either retry layer.
    ///
    /// `allow_empty` controls what an HTTP 200 with an empty body means:
    /// `true` (SendMail/SmartReply/SmartForward per MS-ASCMD) → `Ok(None)`;
    /// `false` (every other command) → `Err(Transport("empty response body"))`.
    ///
    /// `timeout` overrides the client-wide 120s default for this request only
    /// (reqwest `RequestBuilder::timeout`). Ping passes heartbeat + margin so
    /// the server can hold the connection past 120s; `None` keeps the default.
    ///
    /// `accept_multipart` ([MS-ASCMD] §2.2.1.10.1, ItemOperations only):
    /// emits the `MS-ASAcceptMultiPart: T` request header and accepts an
    /// `application/vnd.ms-sync.multipart` response body, resolving its
    /// parts into inline base64 Data before the tree is returned. A
    /// multipart response WITHOUT this opt-in is a protocol violation —
    /// warn-logged and surfaced as a Transport error, never parsed.
    async fn send_command_no_retry(
        &self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        allow_empty: bool,
        timeout: Option<std::time::Duration>,
        accept_multipart: bool,
    ) -> Result<Option<WbxmlElement>, EasError> {
        let wbxml_bytes = serialize_tree(request_root).map_err(EasError::Wbxml)?;
        // Wire-level request dump (debug builds run at DEBUG level): full hex
        // for small bodies (Ping ~100B), capped at 512B for large ones (Sync).
        // Secret-bearing commands (Settings/Provision/ValidateCert) are
        // redacted — see `body_dump_allowed` — so passwords, OOF reply text,
        // and certificate payloads never reach the log even at DEBUG. The
        // placeholder keeps the byte count so the debug session still sees
        // that a body went out, just not its content.
        if body_dump_allowed(cmd_name) {
            log::debug!(
                "EAS {} request body ({} bytes): {}",
                cmd_name,
                wbxml_bytes.len(),
                hex_capped(&wbxml_bytes, 512)
            );
        } else {
            log::debug!(
                "EAS {} request body: <redacted:{}> ({} bytes)",
                cmd_name,
                cmd_name,
                wbxml_bytes.len()
            );
        }

        // Authorization header: prefer the typed EasAuth (OAuth Bearer or
        // Basic-over-enum) when `config.auth` is set; fall back to the
        // historical Basic path built inline from username/password. The
        // fallback preserves the original byte-for-byte header value so
        // existing Basic-auth tests stay green.
        let auth_header = match &self.config.auth {
            Some(auth) => auth.authorization_header().await?,
            None => {
                let auth_value = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", self.config.username, self.config.password));
                format!("Basic {}", auth_value)
            }
        };

        // Query string per [MS-ASHTTP] section 2.1: Cmd + User + DeviceId + DeviceType.
        // Note: the server URL is typically
        // `https://host/Microsoft-Server-ActiveSync` (no trailing slash).
        let url = format!(
            "{}?Cmd={}&User={}&DeviceId={}&DeviceType={}",
            self.config.url.trim_end_matches('/'),
            cmd_name,
            urlencode(self.config.user_param()),
            urlencode(&self.config.device_id),
            urlencode(&self.config.device_type),
        );

        log::debug!("EAS POST {} ({} bytes wbxml)", url, wbxml_bytes.len());

        let mut request = self
            .http
            .post(&url)
            .header("Authorization", &auth_header)
            .header("MS-ASProtocolVersion", &self.config.protocol_version)
            .header("Content-Type", "application/vnd.ms-sync.wbxml")
            .header("Accept", "application/vnd.ms-sync.wbxml")
            .header("X-MS-DeviceType", &self.config.device_type)
            .header("X-MS-DeviceId", &self.config.device_id)
            .header(
                "X-MS-PolicyKey",
                if self.config.policy_key.is_empty() {
                    "0"
                } else {
                    &self.config.policy_key
                },
            )
            .header("User-Agent", &self.config.user_agent)
            .header("Connection", "keep-alive")
            .body(wbxml_bytes);
        // Multipart opt-in ([MS-ASHTTP] §2.2.1.1.2.5): only set when the
        // caller asked for it (ItemOperations with accept_multipart) — an
        // unsolicited header on other commands would invite a response
        // shape they cannot parse.
        if accept_multipart {
            request = request.header("MS-ASAcceptMultiPart", "T");
        }
        // Per-request timeout override (Ping: heartbeat + margin) — wins over
        // the client-wide 120s default set in `EasClient::new`.
        if let Some(d) = timeout {
            request = request.timeout(d);
        }
        let response = request.send().await?;

        let status = response.status().as_u16();

        // Phase 3f Task 5: capture Retry-After (delta-seconds) before we
        // consume the body via `.text()`. HTTP-date form falls back to None
        // (caller uses the default rate-limit window). We use SystemTime (not
        // chrono or SQLite unixepoch()) because the EAS client does not hold a
        // SqlitePool and this is a transport-layer concern — the resulting
        // epoch is compared against SQLite's clock by the engine, which is
        // fine because both read the same wall clock (drift of a few ms is
        // immaterial for a >=60s backoff window).
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| parse_retry_after_delta(s, now_epoch));

        // HTTP 451 carries the new server's full URL in X-MS-Location
        // ([MS-ASHTTP] §2.2.1.1.2.4). Captured (like Retry-After) BEFORE the
        // body is consumed below so the retry layer can adopt the redirect.
        // Header names match case-insensitively via reqwest's HeaderMap.
        let x_ms_location = response
            .headers()
            .get("X-MS-Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let content_type = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        log::debug!(
            "EAS response: status={}, content-type={}",
            status,
            content_type
        );

        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            return Err(EasError::HttpStatus {
                status,
                body,
                retry_after,
                x_ms_location,
            });
        }
        // Check for command-level error in headers (MS-ASProtocolStatus)
        if let Some(proto_status) = response.headers().get("MS-ASProtocolStatus") {
            let s = proto_status.to_str().unwrap_or("0");
            if s != "0" {
                return Err(EasError::CommandStatus {
                    status: s.parse().unwrap_or(0),
                    message: format!("protocol error from server: {}", s),
                });
            }
        }
        // Content-Type branching ([MS-ASCMD] §2.2.1.10.1): inline WBXML is
        // the default; `application/vnd.ms-sync.multipart` is accepted ONLY
        // when this request opted in via MS-ASAcceptMultiPart — multipart
        // without the opt-in is a protocol violation (warn + error, never
        // silently parsed). Anything else is an HTML error page / OWA login.
        let branch = match response_branch(&content_type, accept_multipart) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("EAS {cmd_name}: {e} — the server violated [MS-ASCMD] §2.2.1.10.1");
                return Err(e);
            }
        };
        if branch == ResponseBranch::Unexpected {
            let body = response.bytes().await.unwrap_or_default();
            let preview = String::from_utf8_lossy(&body[..body.len().min(200)]);
            return Err(EasError::Transport(format!(
                "server returned non-WBXML content-type '{}'. First 200 bytes: {}",
                content_type, preview
            )));
        }

        let body = response.bytes().await?;
        if body.is_empty() {
            log::debug!("EAS {} response: empty body", cmd_name);
            // SendMail/SmartReply/SmartForward succeed with an empty body
            // (MS-ASCMD); every other command treats it as an error.
            return match empty_body_outcome(allow_empty) {
                None => Ok(None),
                Some(e) => Err(e),
            };
        }
        // Same redaction gate as the request dump above: the Settings Oof
        // Get RESPONSE carries the user's OOF reply messages, so its body is
        // equally private (task constraint: never log reply content at any
        // level). Byte count stays visible; content does not.
        if body_dump_allowed(cmd_name) {
            log::debug!(
                "EAS {} response body ({} bytes): {}",
                cmd_name,
                body.len(),
                hex_capped(&body, 512)
            );
        } else {
            log::debug!(
                "EAS {} response body: <redacted:{}> ({} bytes)",
                cmd_name,
                cmd_name,
                body.len()
            );
        }

        let root = match branch {
            ResponseBranch::Multipart => {
                // MultiPartResponse envelope ([MS-ASCMD] §2.2.1.10.1.1):
                // part 0 is the WBXML tree; `itemoperations:Part` elements
                // inside airsyncbase:Body reference the later parts
                // (§2.2.3.130). Resolve them into inline base64 Data
                // children so the command parsers see the same tree shape
                // as an inline response.
                let parsed = crate::multipart::parse_multipart_response(&body)?;
                let wbxml_part = parsed.parts.first().ok_or_else(|| {
                    EasError::Transport(
                        "multipart response carries no parts — part 0 must be the WBXML tree"
                            .to_string(),
                    )
                })?;
                let mut tree = match deserialize_to_tree(wbxml_part) {
                    Ok(t) => t,
                    Err(e) => {
                        // Same redaction gate as the inline parse-failure
                        // warn below: the part carries message content.
                        log::warn!(
                            "EAS WBXML parse failed on multipart part 0 ({} bytes, first 64: {}): {}",
                            wbxml_part.len(),
                            parse_failure_preview(wbxml_part, cmd_name),
                            e
                        );
                        return Err(EasError::Wbxml(e));
                    }
                };
                crate::multipart::resolve_part_elements(&mut tree, &parsed.parts)?;
                tree
            }
            ResponseBranch::Wbxml => match deserialize_to_tree(&body) {
                Ok(tree) => tree,
                Err(e) => {
                    // WARN fires in release builds, so the raw-byte preview is
                    // gated like the DEBUG dumps (`parse_failure_preview`):
                    // secret-bearing commands log byte count + parse error only.
                    log::warn!(
                        "EAS WBXML parse failed ({} bytes, first 64: {}): {}",
                        body.len(),
                        parse_failure_preview(&body, cmd_name),
                        e
                    );
                    return Err(EasError::Wbxml(e));
                }
            },
            // Returned early above with the non-WBXML preview error.
            ResponseBranch::Unexpected => {
                return Err(EasError::Transport(format!(
                    "internal error: unexpected content-type '{content_type}' reached deserialization"
                )));
            }
        };
        Ok(Some(root))
    }

    /// Internal no-retry send for the Provision/Settings internals:
    /// empty body is always an error here. `timeout` is the per-request
    /// override threaded down to `send_command_no_retry` (`None` = client
    /// default; the Provision/Settings callers all pass `None`).
    async fn send_command_no_retry_tree(
        &self,
        cmd_name: &str,
        request_root: &WbxmlElement,
        timeout: Option<std::time::Duration>,
    ) -> Result<WbxmlElement, EasError> {
        // Provision/Settings internals never opt in to multipart (the spec
        // defines multipart responses for ItemOperations only).
        self.send_command_no_retry(cmd_name, request_root, false, timeout, false)
            .await?
            .ok_or_else(|| EasError::Transport("empty response body".into()))
    }

    /// HTTP OPTIONS round-trip ([MS-ASHTTP] §2.2.1.1): returns the server's
    /// advertised protocol versions (`MS-ASProtocolVersions`) and supported
    /// command list (`MS-ASProtocolCommands`). No WBXML body — just the
    /// configured URL with auth + User-Agent headers. Used at account setup
    /// to negotiate the protocol version via `pick_protocol_version`.
    ///
    /// Header names are matched case-insensitively by reqwest's `HeaderMap`.
    /// A response carrying NEITHER header is a `Transport` error (the server
    /// is almost certainly not an EAS endpoint); a single missing header
    /// yields an empty list for that side.
    pub async fn options(&self) -> Result<EasServerOptions, EasError> {
        // Same auth-header selection as send_command_no_retry: typed EasAuth
        // when set, else inline Basic from username/password.
        let auth_header = match &self.config.auth {
            Some(auth) => auth.authorization_header().await?,
            None => {
                let auth_value = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", self.config.username, self.config.password));
                format!("Basic {}", auth_value)
            }
        };

        let url = self.config.url.trim_end_matches('/').to_string();
        log::debug!("EAS OPTIONS {}", url);

        let response = self
            .http
            .request(reqwest::Method::OPTIONS, &url)
            .header("Authorization", &auth_header)
            .header("User-Agent", &self.config.user_agent)
            .send()
            .await?;

        let headers = response.headers();
        let versions = headers
            .get("MS-ASProtocolVersions")
            .and_then(|v| v.to_str().ok());
        let commands = headers
            .get("MS-ASProtocolCommands")
            .and_then(|v| v.to_str().ok());
        parse_options_headers(versions, commands)
    }

    /// FolderSync — full folder hierarchy sync.
    pub async fn folder_sync(&mut self, sync_key: &str) -> Result<FolderSyncResult, EasError> {
        let req = commands::build_folder_sync_request(sync_key);
        let resp = self.send_command("FolderSync", &req).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_SYNC)?;
        let result = commands::parse_folder_sync_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "FolderSync failed: {}",
                    commands::folder_sync_status_message(result.status)
                ),
            });
        }
        // Cache the hierarchy key — folder ops must echo it back per MS-ASCMD.
        self.hierarchy_sync_key = result.sync_key.clone();
        Ok(result)
    }

    /// Sync — single-collection item sync.
    ///
    /// The response is parsed through the class-aware seam (M8 Task 4): with
    /// `req.class == "Calendar"` the Add/Change items surface as typed
    /// [`crate::calendar::CalendarEventProps`] on `SyncResult::calendar_added` /
    /// `calendar_updated`; every other class keeps the Email-shaped
    /// `added` / `updated` path bit-for-bit. The REQUEST builder is
    /// class-agnostic either way (`build_sync_request` emits no Class
    /// element in 14.0+).
    pub async fn sync(&mut self, req: &SyncRequest) -> Result<SyncResult, EasError> {
        let tree = commands::build_sync_request(req, &self.config.protocol_version);
        // allow_empty=true: Exchange returns an EMPTY HTTP body for Sync when
        // the collection has nothing to report (Android EasSync.java:225
        // treats empty as OK). None → the no-changes result, which MUST carry
        // the request's sync key (Default's empty key would corrupt the
        // engine's cursor). Some(resp) → the normal parse path.
        match self.send_command_ex("Sync", &tree, true, None).await? {
            None => Ok(no_changes_result(&req.sync_key)),
            Some(resp) => {
                // Verify the server returned a Sync element. A non-Sync root
                // typically means the server returned an error page or OWA
                // redirect that the transport layer couldn't detect — surface
                // it rather than attempt a misleading parse.
                expect_root(&resp, PAGE_AIRSYNC, AS_SYNC)?;
                Ok(commands::parse_sync_response_for_class(&resp, &req.class)?)
            }
        }
    }

    /// Sync — client-side `Commands > Change` upsync for a single collection
    /// (flag mutations etc.). Returns a [`commands::SyncChangeOutcome`]: the
    /// rotated sync key, the collection status, and any server-side Commands
    /// piggybacked onto the response ([MS-ASSYNC] §2.2.2).
    ///
    /// The collection sync key is passed in — `EasClient` deliberately does
    /// NOT cache collection keys (the engine's `eas_sync_state` cursor store
    /// owns them); the caller decides whether to persist the returned key on
    /// success (and must consume the piggybacked changes first if it does).
    ///
    /// Batch size: [MS-ASCMD] §3.1.5.10 SHOULD-limits the Sync command
    /// elements (Add + Change + Delete + Fetch) to
    /// [`SYNC_MAX_COMMANDS_PER_REQUEST`] per request, so batches larger than
    /// the limit are split into sequential ≤200-change chunks. Each chunk's
    /// response rotates the collection sync key — the rotated key is the
    /// REQUIRED request key of the next Sync — so the chunks thread it:
    /// K0 → chunk 1 → K1 → chunk 2 → … The returned outcome carries the LAST
    /// chunk's key and status, with the piggybacked server commands of ALL
    /// chunks merged in chunk order (discarding them while adopting the
    /// rotated key would silently diverge from the server — see
    /// `parse_sync_change_response`).
    ///
    /// Failure is fail-fast: a non-1 collection status (e.g. 3 = invalid
    /// sync key) or transport error in chunk N surfaces as
    /// `EasError::CommandStatus` / the transport error with the chunk number
    /// logged, and chunks N+1.. are NOT sent. The caller (set_flags) never
    /// persists the rotated key on ANY path, so a mid-batch failure leaves
    /// the persisted cursor at the pre-upsync key — the next downsync
    /// re-syncs from there and self-heals; no impossible state is recorded.
    pub async fn sync_changes(
        &mut self,
        collection_id: &str,
        sync_key: &str,
        changes: &[commands::EasChange],
    ) -> Result<commands::SyncChangeOutcome, EasError> {
        let chunks = command_chunks(changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        if chunks.is_empty() {
            // No changes → no round-trip, key unchanged (the production
            // caller already short-circuits on empty batches; this keeps the
            // method total rather than sending an empty Sync command).
            log::debug!(
                "EAS Sync Change: empty batch for collection {collection_id} — no round-trip, sync key unchanged"
            );
            return Ok(commands::SyncChangeOutcome {
                status: 1,
                new_key: sync_key.to_string(),
                ..Default::default()
            });
        }
        let total_chunks = chunks.len();
        let mut merged = commands::SyncChangeOutcome {
            status: 1,
            new_key: sync_key.to_string(),
            ..Default::default()
        };
        let mut current_key = sync_key.to_string();
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let chunk_no = chunk_idx + 1;
            let outcome = match self
                .sync_changes_chunk(chunk_no, total_chunks, collection_id, &current_key, chunk)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    // Fail-fast: later chunks are NOT sent after a failure —
                    // the batch is already partially applied server-side and
                    // continuing would only widen the partial window.
                    let done: usize = chunks[..chunk_idx].iter().map(|c| c.len()).sum();
                    log::warn!(
                        "EAS Sync Change upsync aborted at chunk {chunk_no}/{total_chunks} for collection {collection_id}: {e} — {done} changes already applied in earlier chunks, the failing chunk carried {}; remaining chunks NOT sent (fail-fast). The persisted cursor is untouched (set_flags never persists the rotated key), so the next downsync re-syncs from the pre-upsync key and self-heals.",
                        chunk.len()
                    );
                    return Err(e);
                }
            };
            log::debug!(
                "EAS Sync Change chunk {chunk_no}/{total_chunks} for collection {collection_id}: {} changes upsynced, sync key rotated{}",
                chunk.len(),
                if outcome.has_piggybacked() {
                    " (response carries piggybacked server commands)"
                } else {
                    ""
                }
            );
            // Thread the rotated key into the next chunk and merge this
            // chunk's piggybacked server commands in chunk order.
            current_key = outcome.new_key.clone();
            merged.piggybacked_added.extend(outcome.piggybacked_added);
            merged
                .piggybacked_updated
                .extend(outcome.piggybacked_updated);
            merged
                .piggybacked_deleted
                .extend(outcome.piggybacked_deleted);
            merged.new_key = outcome.new_key;
            merged.status = outcome.status;
        }
        Ok(merged)
    }

    /// One ≤[`SYNC_MAX_COMMANDS_PER_REQUEST`]-change Sync round-trip. The
    /// chunk numbers are embedded in the surfaced `CommandStatus` message so
    /// a mid-batch failure is diagnosable from the caller's log alone.
    async fn sync_changes_chunk(
        &mut self,
        chunk_no: usize,
        total_chunks: usize,
        collection_id: &str,
        sync_key: &str,
        changes: &[commands::EasChange],
    ) -> Result<commands::SyncChangeOutcome, EasError> {
        let tree = commands::build_sync_change_request(collection_id, sync_key, changes);
        let resp = self.send_command("Sync", &tree).await?;
        expect_root(&resp, PAGE_AIRSYNC, AS_SYNC)?;
        let outcome = commands::parse_sync_change_response(&resp)?;
        if outcome.status != 1 {
            return Err(EasError::CommandStatus {
                status: outcome.status,
                message: format!(
                    "Sync Change failed (chunk {chunk_no}/{total_chunks}): {}",
                    commands::common_status_message(outcome.status)
                        .unwrap_or("collection status not success")
                ),
            });
        }
        Ok(outcome)
    }

    /// SendMail — send a single MIME message. Success per MS-ASCMD is an
    /// HTTP 200 with an EMPTY body (we return status 1); a WBXML body is
    /// only present on failure and carries the Status.
    pub async fn send_mail(&mut self, req: &SendMailRequest) -> Result<u32, EasError> {
        let tree = commands::build_send_mail_request(req);
        match self.send_command_ex("SendMail", &tree, true, None).await? {
            None => Ok(1),
            Some(resp) => {
                expect_root(&resp, PAGE_COMPOSE, CM_SEND_MAIL)?;
                Ok(commands::parse_send_mail_response(&resp)?)
            }
        }
    }

    /// SmartForward — forward an existing server-side message with new MIME body.
    /// Same empty-body-success contract as SendMail (MS-ASCMD).
    ///
    /// Degradation: when the server rejects the SmartForward we log and fall
    /// back to plain SendMail with the same MIME — the frontend already sends
    /// a complete RFC 5322 message, so SendMail alone carries everything the
    /// user composed. Rejection has two shapes, BOTH degraded:
    ///   1. `EasError::CommandStatus` (transport/header-level rejection, e.g. the source message's
    ///      attachments are gone);
    ///   2. HTTP 200 + in-body `<Status> != 1` — the common EAS compose rejection shape;
    ///      `parse_send_mail_response` surfaces it as `Ok(status)`, NOT an Err, so it needs its own
    ///      arm. Transport / HTTP-status / WBXML errors are NOT degraded (the SmartForward may
    ///      still succeed on retry) and propagate unchanged.
    pub async fn smart_forward(&mut self, req: &SmartForwardRequest) -> Result<u32, EasError> {
        let tree = commands::build_smart_forward_request(req)?;
        let result = match self
            .send_command_ex("SmartForward", &tree, true, None)
            .await
        {
            Ok(None) => Ok(1),
            Ok(Some(resp)) => {
                expect_root(&resp, PAGE_COMPOSE, CM_SMART_FORWARD)?;
                Ok(commands::parse_send_mail_response(&resp)?)
            }
            Err(e) => Err(e),
        };
        match result {
            Err(e) if should_degrade_to_send_mail(&e) => {
                log::info!("EAS SmartForward rejected ({e}) — degrading to plain SendMail");
                self.smart_forward_degrade_to_send_mail(req).await
            }
            Ok(status) if smart_forward_inbody_status_failed(status) => {
                log::info!(
                    "EAS SmartForward returned in-body status {status} — degrading to plain SendMail"
                );
                self.smart_forward_degrade_to_send_mail(req).await
            }
            other => other,
        }
    }

    /// Shared fallback for both SmartForward rejection shapes: decode the
    /// base64 MIME (`SendMailRequest` carries raw RFC 5322 bytes in an OPAQUE
    /// `<Mime>`, while `SmartForwardRequest` holds base64 text) and send it
    /// as plain SendMail. `save_to_sent` carries over; `client_id` carries
    /// over too, and is SYNTHESIZED when the caller didn't set one — Exchange
    /// 15.2 rejects ClientId-less compose requests with in-body Status 103
    /// (F10-3 live evidence: the pre-fix fallback always passed `None`, so a
    /// SmartForward rejection could never be rescued by degrade on this
    /// server). The synthesized id goes through
    /// `types::new_send_client_id` so it always fits the [MS-ASCMD] 40-char
    /// ClientId cap (task-11 live evidence: the previous
    /// `KylinsSmartForwardDegrade-{nanos}` form was ~55 chars → Status 103).
    async fn smart_forward_degrade_to_send_mail(
        &mut self,
        req: &SmartForwardRequest,
    ) -> Result<u32, EasError> {
        let mime = base64::engine::general_purpose::STANDARD
            .decode(&req.mime_base64)
            .map_err(|err| {
                EasError::Transport(format!("SmartForward mime_base64 decode failed: {err}"))
            })?;
        let client_id = req
            .client_id
            .clone()
            .or_else(|| Some(crate::types::new_send_client_id("SFWD-")));
        let send_req = SendMailRequest {
            mime,
            save_to_sent: req.save_to_sent,
            client_id,
        };
        self.send_mail(&send_req).await
    }

    /// SmartReply — reply to an existing server-side message with new MIME body.
    /// Same empty-body-success contract as SendMail (MS-ASCMD).
    pub async fn smart_reply(&mut self, req: &SmartReplyRequest) -> Result<u32, EasError> {
        let tree = commands::build_smart_reply_request(req)?;
        match self
            .send_command_ex("SmartReply", &tree, true, None)
            .await?
        {
            None => Ok(1),
            Some(resp) => {
                expect_root(&resp, PAGE_COMPOSE, CM_SMART_REPLY)?;
                Ok(commands::parse_send_mail_response(&resp)?)
            }
        }
    }

    /// ItemOperations — fetch an attachment or item by server id. When
    /// `req.accept_multipart` is set the request carries
    /// `MS-ASAcceptMultiPart: T` and a multipart response ([MS-ASCMD]
    /// §2.2.1.10.1) is accepted and resolved inline before parsing, so the
    /// result shape is identical either way.
    pub async fn item_operations(
        &mut self,
        req: &ItemOperationsFetchRequest,
    ) -> Result<ItemOperationsFetchResult, EasError> {
        let tree = commands::build_item_operations_request(req);
        let resp = self
            .send_command_ex_opts("ItemOperations", &tree, false, None, req.accept_multipart)
            .await?
            .ok_or_else(|| EasError::Transport("empty response body".into()))?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        Ok(commands::parse_item_operations_response(&resp)?)
    }

    /// ItemOperations → EmptyFolderContents ([MS-ASCMD] §4.14.4): deletes
    /// EVERY item in the named folder server-side (and, when
    /// `req.delete_sub_folders` is set, the folder's subfolders too). A
    /// standalone user-facing command, so — like `settings_user_information`
    /// — it goes through the normal retry path.
    ///
    /// DESTRUCTIVE ACTION: this wipes a folder's contents on the server and
    /// cannot be undone from the client. Callers MUST confirm with the user
    /// before invoking. Errors carry protocol status codes only — no folder
    /// or item data is interpolated into logs or error messages.
    ///
    /// `result.status` is the effective status (top-level itemoperations
    /// Status, overridden by the EmptyFolderContents-level Status when
    /// present — the parser's more-specific-wins rule). Non-1 is surfaced
    /// as a typed CommandStatus error, mirroring the Settings family.
    pub async fn empty_folder_contents(
        &mut self,
        req: &EmptyFolderContentsRequest,
    ) -> Result<EmptyFolderContentsResult, EasError> {
        let tree = commands::build_empty_folder_contents_request(req);
        let resp = self.send_command("ItemOperations", &tree).await?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        let result = commands::parse_empty_folder_contents_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ItemOperations EmptyFolderContents failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// ItemOperations → Move ([MS-ASCMD] §4.25): moves a whole conversation
    /// to another folder. When `req.move_always` is set, ALL FUTURE messages
    /// of the conversation are moved too — a persistent server-side behavior
    /// the caller MUST surface to the user before invoking. A standalone
    /// user-facing command, so — like `empty_folder_contents` — it goes
    /// through the normal retry path.
    ///
    /// `req.conversation_id` is an opaque server blob carried verbatim
    /// (never decoded). `result.status` is the effective status (top-level
    /// itemoperations Status, overridden by the Move-level Status when
    /// present — the parser's more-specific-wins rule). Non-1 is surfaced
    /// as a typed CommandStatus error, mirroring the Settings family.
    pub async fn conversation_move(
        &mut self,
        req: &ConversationMoveRequest,
    ) -> Result<ConversationMoveResult, EasError> {
        let tree = commands::build_conversation_move_request(req);
        let resp = self.send_command("ItemOperations", &tree).await?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        let result = commands::parse_conversation_move_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ItemOperations Move failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// MoveItems — move one or more messages between folders
    /// ([MS-ASCMD] §2.2.1.12). `moves` is a batch of
    /// `(src_msg_id, src_fld_id, dst_fld_id)` tuples (wire ServerIds / folder
    /// ServerIds). Returns per-Move `(Status, DstMsgId)` pairs merged in
    /// request order on full success.
    ///
    /// Batch size: [MS-ASCMD] §3.1.5.10 SHOULD-limits a MoveItems request to
    /// [`MOVE_ITEMS_MAX_PER_COMMAND`] Move elements, so batches larger than
    /// the limit are split into sequential ≤1000-move commands and the
    /// per-move results merged in order (`command_chunks` covers the input
    /// contiguously, keeping merged results aligned with the request tuples).
    ///
    /// Batch-failure policy (fail-fast): the FIRST per-Move result that
    /// `commands::move_status_succeeded` rejects in ANY chunk surfaces as
    /// `EasError::CommandStatus` (decoded by `move_items_status_message` —
    /// [MS-ASCMD] 2.2.3.177.10), and LATER CHUNKS ARE NOT SENT — the batch is
    /// already partially applied server-side at that point; continuing would
    /// only widen the partial-batch window, and the caller reconciles via the
    /// next sync rounds either way. Every chunk's outcome is logged; nothing
    /// is swallowed. Note the MoveItems status table is INVERTED versus every
    /// other command: **3 is the success code** (returned with a valid
    /// DstMsgId — Exchange 15.2 live evidence, F10-2), and 1 means "invalid
    /// source collection/item ID".
    pub async fn move_items(
        &mut self,
        moves: &[(String, String, String)],
    ) -> Result<Vec<(u32, Option<String>)>, EasError> {
        let chunks = command_chunks(moves, MOVE_ITEMS_MAX_PER_COMMAND);
        if chunks.is_empty() {
            // No moves → no round-trip (the production caller already
            // short-circuits on empty batches; this keeps the method total
            // rather than sending an empty MoveItems command).
            log::debug!("EAS MoveItems: empty batch — no round-trip");
            return Ok(Vec::new());
        }
        let total_chunks = chunks.len();
        let mut results: Vec<(u32, Option<String>)> = Vec::with_capacity(moves.len());
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let chunk_no = chunk_idx + 1;
            match self.move_items_chunk(chunk_no, total_chunks, chunk).await {
                Ok(chunk_results) => {
                    log::debug!(
                        "EAS MoveItems chunk {chunk_no}/{total_chunks}: {} moves succeeded",
                        chunk_results.len()
                    );
                    // Chunks cover the input contiguously in order, so this
                    // keeps the merged pairs aligned with the request tuples.
                    results.extend(chunk_results);
                }
                Err(e) => {
                    // Fail-fast: later chunks are NOT sent after a failure.
                    let done: usize = chunks[..chunk_idx].iter().map(|c| c.len()).sum();
                    log::warn!(
                        "EAS MoveItems aborted at chunk {chunk_no}/{total_chunks}: {e} — {done} moves already applied in earlier chunks, the failing chunk carried {}; remaining chunks NOT sent (fail-fast avoids widening the partial-batch window)",
                        chunk.len()
                    );
                    return Err(e);
                }
            }
        }
        Ok(results)
    }

    /// One ≤[`MOVE_ITEMS_MAX_PER_COMMAND`]-move MoveItems round-trip: build,
    /// send, parse, and gate the per-move statuses. The chunk numbers are
    /// embedded in the surfaced `CommandStatus` message so a mid-batch
    /// failure is diagnosable from the caller's log alone.
    async fn move_items_chunk(
        &mut self,
        chunk_no: usize,
        total_chunks: usize,
        moves: &[(String, String, String)],
    ) -> Result<Vec<(u32, Option<String>)>, EasError> {
        let tree = commands::build_move_items_request(moves);
        let resp = self.send_command("MoveItems", &tree).await?;
        expect_root(&resp, PAGE_MOVE, MV_MOVE_ITEMS)?;
        let results = commands::parse_move_items_response(&resp)?;
        // F10-2: per [MS-ASCMD] 2.2.3.177.10 the MoveItems SUCCESS status is
        // 3, not 1 — Exchange 15.2 returns per-Move Status 3 WITH a valid
        // DstMsgId and performs the move (IMAP-verified 2026-08-02). Log at
        // debug (this fires on every successful 15.2 move — warn would spam);
        // the gate below tolerates it (see `move_status_succeeded`).
        for (status, dst_msg_id) in &results {
            if *status == 3 && dst_msg_id.is_some() {
                log::debug!(
                    "EAS MoveItems: per-Move Status 3 with DstMsgId {dst_msg_id:?} — per [MS-ASCMD] 2.2.3.177.10, 3 is the MoveItems SUCCESS code (not 1); the server performed the move and returned the new item id. Treating as success."
                );
            }
        }
        if let Some(status) = commands::first_failing_move_status(&results) {
            return Err(EasError::CommandStatus {
                status,
                message: format!(
                    "MoveItems failed (chunk {chunk_no}/{total_chunks}): {}",
                    commands::move_items_status_message(status)
                ),
            });
        }
        Ok(results)
    }

    /// MeetingResponse — accept/tentative/decline a meeting request
    /// ([MS-ASCMD] §2.2.1.11). `collection_id` is the folder holding the
    /// invite email; `request_id` is that EMAIL message's wire ServerId;
    /// `user_response` is "1" (accept) / "2" (tentative) / "3" (decline).
    /// `instance_id`, when Some, names ONE instance of a recurring meeting
    /// (its original UTC start time, [MS-ASCAL]-format — §2.2.3.92.1); None
    /// applies the response to every instance. `send_response` emits the
    /// empty `<SendResponse/>` element asking a 16.0/16.1 server to email
    /// the organizer (the token is unregistered on older protocol versions —
    /// the IPC layer gates it). A non-1 Result Status surfaces as
    /// `EasError::CommandStatus` (decoded by
    /// `meeting_response_status_message` — [MS-ASCMD] 2.2.3.177.9).
    pub async fn meeting_response(
        &mut self,
        collection_id: &str,
        request_id: &str,
        user_response: &str,
        instance_id: Option<&str>,
        send_response: bool,
    ) -> Result<u32, EasError> {
        let tree = commands::build_meeting_response_request(
            collection_id,
            request_id,
            user_response,
            instance_id,
            send_response,
        );
        let resp = self.send_command("MeetingResponse", &tree).await?;
        expect_root(&resp, PAGE_MREQ, MREQ_MEETING_RESPONSE)?;
        let status = commands::parse_meeting_response_response(&resp)?;
        if status != 1 {
            return Err(EasError::CommandStatus {
                status,
                message: format!(
                    "MeetingResponse failed: {}",
                    commands::meeting_response_status_message(status)
                ),
            });
        }
        Ok(status)
    }

    /// GetItemEstimate — count of pending items for a collection.
    pub async fn get_item_estimate(
        &mut self,
        req: &GetItemEstimateRequest,
    ) -> Result<GetItemEstimateResult, EasError> {
        let tree = commands::build_get_item_estimate_request(req);
        let resp = self.send_command("GetItemEstimate", &tree).await?;
        // Root page for GetItemEstimate is 6; root token 0x05.
        expect_root(&resp, 6, GIE_ROOT)?;
        Ok(commands::parse_get_item_estimate_response(&resp)?)
    }

    /// Search — mailbox or GAL search ([MS-ASCMD] §2.2.1.16).
    pub async fn search(&mut self, req: &SearchRequest) -> Result<SearchResult, EasError> {
        let tree = commands::build_search_request(req);
        let resp = self.send_command("Search", &tree).await?;
        expect_root(&resp, PAGE_SEARCH, SR_SEARCH)?;
        Ok(commands::parse_search_response(&resp)?)
    }

    /// Ping — block up to heartbeat_interval waiting for changes.
    ///
    /// Per-request timeout: the client-wide reqwest default is 120s, which
    /// otherwise kills every server-held ping at 120s while the tuned
    /// heartbeat can reach 480s (cap 1680s) — log-verified as transport
    /// failures causing strikes → drop to poll. Both the initial request and
    /// the status-5 retry pass `heartbeat + 60s` via `ping_request_timeout`
    /// (the retry uses the server-adopted interval).
    pub async fn ping(&mut self, req: &PingRequest) -> Result<PingResult, EasError> {
        let started = std::time::Instant::now();
        let collections: Vec<&str> = req
            .monitored_collections
            .iter()
            .map(|c| c.collection_id.as_str())
            .collect();
        // INFO-level wire summary: the ping request/response is observable in
        // the app log without DEBUG builds (full WBXML hex stays at DEBUG).
        log::info!(
            "EAS Ping → heartbeat={}s collections={:?}",
            req.heartbeat_interval,
            collections
        );
        let tree = commands::build_ping_request(req);
        let timeout = Some(ping_request_timeout(req.heartbeat_interval));
        let resp = self.send_command_timed("Ping", &tree, timeout).await?;
        expect_root(&resp, PAGE_PING, PING_PING)?;
        let result = commands::parse_ping_response(&resp)?;
        log::info!(
            "EAS Ping ← held={:.1}s status={} heartbeat_interval={:?} folders={:?}",
            started.elapsed().as_secs_f64(),
            result.status,
            result.heartbeat_interval,
            result.folders
        );
        if let Some(interval) = ping_retry_interval(&result.status, result.heartbeat_interval) {
            log::info!(
                "EAS Ping status 5 — retrying once with server heartbeat {interval}s (was {}s)",
                req.heartbeat_interval
            );
            let retry_req = PingRequest {
                heartbeat_interval: interval,
                monitored_collections: req.monitored_collections.clone(),
            };
            let retry_tree = commands::build_ping_request(&retry_req);
            let retry_timeout = Some(ping_request_timeout(interval));
            let retry_started = std::time::Instant::now();
            let retry_resp = self
                .send_command_timed("Ping", &retry_tree, retry_timeout)
                .await?;
            expect_root(&retry_resp, PAGE_PING, PING_PING)?;
            let mut retry_result = commands::parse_ping_response(&retry_resp)?;
            log::info!(
                "EAS Ping ← (retry) held={:.1}s status={} heartbeat_interval={:?} folders={:?}",
                retry_started.elapsed().as_secs_f64(),
                retry_result.status,
                retry_result.heartbeat_interval,
                retry_result.folders
            );
            // Surface the adopted server interval so the engine's ping loop
            // can tune + persist it (the retry's own response carries no
            // HeartbeatInterval element unless it too is a status 5).
            retry_result.adopted_heartbeat = Some(interval);
            return Ok(retry_result);
        }
        Ok(result)
    }

    /// FolderCreate — create a new folder under a parent.
    pub async fn folder_create(
        &mut self,
        req: &FolderCreateRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_create_request(req, &key);
        let resp = self.send_command("FolderCreate", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_CREATE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// FolderDelete — delete a folder by server id.
    pub async fn folder_delete(
        &mut self,
        req: &FolderDeleteRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_delete_request(req, &key);
        let resp = self.send_command("FolderDelete", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_DELETE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// FolderUpdate — rename or move a folder.
    pub async fn folder_update(
        &mut self,
        req: &FolderUpdateRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_update_request(req, &key);
        let resp = self.send_command("FolderUpdate", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_UPDATE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// Adopt the hierarchy SyncKey a folder-op response carries ([MS-ASCMD]
    /// 2.2.3.181.1): every FolderCreate/Update/Delete advances the hierarchy
    /// key, and the next folder op must send the new one — without this a
    /// create→delete sequence goes out with a stale key (live evidence:
    /// eas_folder_debug 2026-08-02, delete with pre-create key → status 110).
    fn adopt_folder_op_sync_key(&mut self, resp: &WbxmlElement) {
        if let Some(key) = commands::folder_op_response_sync_key(resp)
            && !key.is_empty()
        {
            self.hierarchy_sync_key = key;
        }
    }

    /// Settings → DeviceInformation (MS-ASCMD §2.2.1.18): identifies this
    /// device's model/OS so the server can evaluate provisioning policy.
    /// Sent on demand when Provision answers 165 (DeviceInformationRequired).
    /// Uses `send_command_no_retry` for the same anti-recursion invariant as
    /// `provision()` (it is called FROM `provision()`).
    pub async fn settings_device_information(&mut self) -> Result<(), EasError> {
        let req = commands::build_settings_device_information_request(
            &self.config.device_type,
            "Kylins Mail desktop",
            std::env::consts::OS,
            "en-US",
        );
        let resp = self
            .send_command_no_retry_tree("Settings", &req, None)
            .await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::SETTINGS,
            crate::wbxml::tags::settings::SETTINGS,
        )?;
        let (top, di) = commands::parse_settings_response(&resp)?;
        if top != 1 {
            return Err(EasError::CommandStatus {
                status: top,
                message: format!(
                    "Settings failed: {}",
                    commands::common_status_message(top).unwrap_or("unknown status code")
                ),
            });
        }
        if di != 1 {
            return Err(EasError::CommandStatus {
                status: di,
                message: format!("Settings DeviceInformation rejected (status {di})"),
            });
        }
        Ok(())
    }

    /// Settings → UserInformation, Get form ([MS-ASCMD] §4.21): returns the
    /// mailbox's SMTP addresses (identity confirmation / account setup).
    /// Unlike `settings_device_information` — which goes through
    /// `send_command_no_retry_tree` because it is called FROM `provision()` —
    /// this is a standalone user-facing command, so it uses the normal retry
    /// path like every other frontend-invoked command.
    pub async fn settings_user_information(&mut self) -> Result<UserInformationResult, EasError> {
        let req = commands::build_settings_user_information_request();
        let resp = self.send_command("Settings", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::SETTINGS,
            crate::wbxml::tags::settings::SETTINGS,
        )?;
        let result = commands::parse_settings_user_information_response(&resp)?;
        // `result.status` is the effective status (top-level Settings Status,
        // overridden by the UserInformation-level Status when present — the
        // parser's more-specific-wins rule). Non-1 is surfaced as a typed
        // CommandStatus error, mirroring `settings_device_information`.
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "Settings UserInformation failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// Settings → DevicePassword, Set form ([MS-ASCMD] §4.22): stores the
    /// device's recovery password in the user's mailbox (the server's
    /// provisioning policy may require one when device-password enforcement
    /// is active). A standalone user-facing command, so — like
    /// `settings_user_information` — it goes through the normal retry path.
    ///
    /// SECURITY: `password` is the device unlock/recovery password. It
    /// travels to the server over TLS only and is NEVER logged, persisted,
    /// or interpolated into any log or error message here; errors carry only
    /// the protocol status code. Do not add logging that could include it.
    pub async fn settings_device_password(
        &mut self,
        password: &str,
    ) -> Result<DevicePasswordResult, EasError> {
        let req = commands::build_settings_device_password_request(password);
        let resp = self.send_command("Settings", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::SETTINGS,
            crate::wbxml::tags::settings::SETTINGS,
        )?;
        let result = commands::parse_settings_device_password_response(&resp)?;
        // `result.status` is the effective status (top-level Settings Status,
        // overridden by the DevicePassword-level Status when present — the
        // parser's more-specific-wins rule). Non-1 is surfaced as a typed
        // CommandStatus error, mirroring `settings_user_information`.
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "Settings DevicePassword failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// Settings → Oof, Get form ([MS-ASCMD] §4.19.1): retrieves the user's
    /// out-of-office state, scheduled window, and per-audience reply
    /// messages. `body_type` is the format the server returns the messages
    /// in ("Text" or "HTML", §2.2.3.17). A standalone user-facing command,
    /// so — like `settings_user_information` — it goes through the normal
    /// retry path.
    ///
    /// SECURITY: the returned `reply_message` strings are private user
    /// content. They are never logged here; the transport layer's DEBUG
    /// body dump is redacted for the Settings command (see
    /// `body_dump_allowed` in this module).
    pub async fn settings_oof_get(&mut self, body_type: &str) -> Result<OofSettings, EasError> {
        let req = commands::build_settings_oof_get_request(body_type);
        let resp = self.send_command("Settings", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::SETTINGS,
            crate::wbxml::tags::settings::SETTINGS,
        )?;
        let (settings, status) = commands::parse_settings_oof_get_response(&resp)?;
        // `status` is the effective status (top-level Settings Status,
        // overridden by the Oof-level Status when present — the parser's
        // more-specific-wins rule). Non-1 is surfaced as a typed
        // CommandStatus error, mirroring `settings_user_information`.
        if status != 1 {
            return Err(EasError::CommandStatus {
                status,
                message: format!(
                    "Settings Oof Get failed: {}",
                    commands::common_status_message(status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(settings)
    }

    /// Settings → Oof, Set form ([MS-ASCMD] §4.19.2): updates the user's
    /// out-of-office state, scheduled window, and per-audience reply
    /// messages. A standalone user-facing command, so — like
    /// `settings_user_information` — it goes through the normal retry path.
    ///
    /// SECURITY: `settings.messages[].reply_message` is private user
    /// content. It travels to the server over TLS only and is NEVER logged
    /// or interpolated into any log or error message here; errors carry
    /// only the protocol status code. The transport layer's DEBUG body dump
    /// is redacted for the Settings command (see `body_dump_allowed`).
    pub async fn settings_oof_set(
        &mut self,
        settings: &OofSettings,
    ) -> Result<OofResult, EasError> {
        let req = commands::build_settings_oof_set_request(settings);
        let resp = self.send_command("Settings", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::SETTINGS,
            crate::wbxml::tags::settings::SETTINGS,
        )?;
        let result = commands::parse_settings_oof_set_response(&resp)?;
        // `result.status` is the effective status (top-level Settings
        // Status, overridden by the Oof-level Status when present — the
        // parser's more-specific-wins rule). Non-1 is surfaced as a typed
        // CommandStatus error, mirroring `settings_user_information`.
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "Settings Oof Set failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// ValidateCert ([MS-ASCMD] §2.2.1.22 / §4.20): asks the server to
    /// validate one or more X.509 certificates (S/MIME signature
    /// verification) — expiry, revocation, and chain walk to a trusted root.
    /// Supported on every protocol version (§2.2.1.22). A standalone
    /// user-facing command, so — like `settings_user_information` — it goes
    /// through the normal retry path.
    ///
    /// The command-level status (§2.2.3.177.18: 1 = success, 17 = failure)
    /// gates the result: non-1 surfaces as a typed CommandStatus error with
    /// the common-status message context, mirroring the Settings family. The
    /// per-certificate statuses ride on the returned
    /// [`ValidateCertResult::certificate_statuses`] (response order) — a
    /// non-1 per-cert code is a VALIDATION verdict, not a protocol error,
    /// and the caller decides what it means.
    ///
    /// SECURITY: the request carries opaque base64 DER certificate payloads
    /// — large and security-sensitive material. They are NEVER logged or
    /// interpolated into any log or error message here; errors carry only
    /// the protocol status code. The transport layer's DEBUG body dumps are
    /// redacted for this command (see `body_dump_allowed`).
    pub async fn validate_cert(
        &mut self,
        request: &ValidateCertRequest,
    ) -> Result<ValidateCertResult, EasError> {
        let req = commands::build_validate_cert_request(request);
        let resp = self.send_command("ValidateCert", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::VALIDATE,
            crate::wbxml::tags::validatecert::VALIDATE_CERT,
        )?;
        let result = commands::parse_validate_cert_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ValidateCert failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// ResolveRecipients ([MS-ASCMD] §2.2.1.15 / §4.18): resolves a list of
    /// ambiguous-name (ANR) strings and/or SMTP addresses to directory
    /// entries (GAL + contacts), optionally fetching free/busy data over
    /// `request.availability`. A standalone user-facing command, so — like
    /// `settings_user_information` — it goes through the normal retry path.
    ///
    /// The command-level status (§2.2.3.177.12: 1 = success, 5 = protocol
    /// error, 6 = server error) gates the result: non-1 surfaces as a typed
    /// CommandStatus error with the common-status message context,
    /// mirroring the ValidateCert/Settings family. Per-To statuses (2/3
    /// ambiguous, 4 no match) and per-recipient availability codes (160 /
    /// 161 / 162) are DATA riding on the returned
    /// [`ResolveRecipientsResult::responses`] — the caller prompts the user
    /// or retries per code; they are not protocol errors.
    ///
    /// An EMPTY `to` list is rejected client-side before any network I/O:
    /// §2.2.3.191 requires at least one To, and an empty request resolves
    /// nothing.
    ///
    /// PRIVACY: the request carries directory lookup strings and the
    /// response carries directory PII (names, SMTP addresses) plus
    /// free/busy data. None of it is logged here — errors carry the
    /// protocol status code only — and the transport layer's DEBUG body
    /// dumps are redacted for this command (see `body_dump_allowed`).
    pub async fn resolve_recipients(
        &mut self,
        request: &ResolveRecipientsRequest,
    ) -> Result<ResolveRecipientsResult, EasError> {
        if request.to.is_empty() {
            // REJECT, never send: a ResolveRecipients with no To is
            // pointless, and emitting one wastes a round-trip on a certain
            // protocol error. `InvalidRequest`, not `CommandStatus` — the
            // server never spoke (no network I/O happens on this path).
            return Err(EasError::InvalidRequest(
                "ResolveRecipients requires at least one To entry — empty recipient list rejected"
                    .to_string(),
            ));
        }
        let req = commands::build_resolve_recipients_request(request);
        let resp = self.send_command("ResolveRecipients", &req).await?;
        expect_root(
            &resp,
            crate::wbxml::tags::pages::RECIPIENTS,
            crate::wbxml::tags::recipients::RESOLVE_RECIPIENTS,
        )?;
        let result = commands::parse_resolve_recipients_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ResolveRecipients failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// Provision phase 1: request the policy. Returns the parsed response
    /// (caller inspects status/policy_key/remote_wipe). The request embeds
    /// `<DeviceInformation>` (Settings page 18) as its first child — the same
    /// values `settings_device_information` sends — because Exchange 2019
    /// demands DI inline (status 165) and gates the standalone Settings
    /// command on provisioning (status 142).
    async fn provision_phase1(&mut self) -> Result<crate::provision::ProvisionResult, EasError> {
        let req = crate::provision::build_provision_phase1_request(
            &self.config.device_type,
            "Kylins Mail desktop",
            std::env::consts::OS,
            "en-US",
        );
        let resp = self
            .send_command_no_retry_tree("Provision", &req, None)
            .await?;
        Ok(crate::provision::parse_provision_response(&resp)?)
    }

    /// Run the two-phase Provision handshake (MS-ASPROV) and persist the
    /// resulting permanent policy key into `self.config.policy_key`.
    /// Subsequent commands then send it via the X-MS-PolicyKey header (already
    /// wired in `send_command`).
    ///
    /// Takes `&mut self` because Phase 2 writes the permanent key. The other
    /// command methods also take `&mut self` now that `send_command` does —
    /// Provision is no longer unique in that regard. The Provision/Settings
    /// internals (`provision_phase1`, phase 2, `settings_device_information`)
    /// go through `send_command_no_retry` directly to avoid recursing back
    /// into the retry wrapper's `RunProvision` branch, which calls
    /// `provision()`.
    ///
    /// Errors with `CommandStatus { status: 140, ... }` if either phase
    /// returns a `<RemoteWipe>` element — we surface, NEVER auto-execute
    /// (per Global Constraints). Other non-1 statuses surface as
    /// `CommandStatus` with the protocol status code.
    pub async fn provision(&mut self) -> Result<(), EasError> {
        // Phase 1: request the policy. Server returns a temp PolicyKey + the
        // policy XML in <Data>.
        //
        // IMPORTANT: provision() is invoked by the retry wrapper's
        // `RunProvision` branch. It MUST send via `send_command_no_retry`
        // (never the retry wrapper) so a 449 during the Provision handshake
        // surfaces instead of recursing into `provision()` again.
        let parsed1 = self.provision_phase1().await?;
        // Status 165 = DeviceInformationRequired: the server won't issue a
        // policy until the client identifies itself. Phase 1 already embeds
        // DeviceInformation inline (the primary path); for servers that still
        // answer 165 (e.g. old-protocol flows), send it once via the
        // standalone Settings command, then retry phase 1 once.
        let parsed1 = if parsed1.status == 165 {
            log::info!(
                "EAS Provision answered 165 (DeviceInformationRequired) — sending Settings DeviceInformation and retrying once"
            );
            self.settings_device_information().await?;
            self.provision_phase1().await?
        } else {
            parsed1
        };
        if parsed1.remote_wipe {
            return Err(EasError::CommandStatus {
                status: 140,
                message: "server requested RemoteWipe — refusing to auto-execute".into(),
            });
        }
        if parsed1.status != 1 {
            return Err(EasError::CommandStatus {
                status: parsed1.status,
                message: format!("Provision phase 1 status {}", parsed1.status),
            });
        }
        let temp_key = parsed1
            .policy_key
            .ok_or_else(|| EasError::Transport("Provision phase 1 returned no PolicyKey".into()))?;

        // Phase 2: ack with the temp key and Status 1 (client compliant).
        // Server replies with the permanent PolicyKey. Uses
        // `send_command_no_retry` for the same anti-recursion reason as phase 1.
        let req2 = crate::provision::build_provision_phase2_request(&temp_key);
        let resp2 = self
            .send_command_no_retry_tree("Provision", &req2, None)
            .await?;
        let parsed2 = crate::provision::parse_provision_response(&resp2)?;
        if parsed2.remote_wipe {
            return Err(EasError::CommandStatus {
                status: 140,
                message: "server requested RemoteWipe in phase 2 — refusing".into(),
            });
        }
        if parsed2.status != 1 {
            return Err(EasError::CommandStatus {
                status: parsed2.status,
                message: format!("Provision phase 2 status {}", parsed2.status),
            });
        }
        let perm_key = parsed2.policy_key.ok_or_else(|| {
            EasError::Transport("Provision phase 2 returned no permanent PolicyKey".into())
        })?;
        self.config.policy_key = perm_key;
        Ok(())
    }
}

fn expect_root(root: &WbxmlElement, page: u8, token: u8) -> Result<(), EasError> {
    if root.page == page && root.token == token {
        Ok(())
    } else {
        Err(EasError::UnexpectedRoot {
            page: root.page,
            token: root.token,
        })
    }
}

/// Pick the protocol version to negotiate with the server. Ports Android's
/// EasOptions algorithm: the server's `MS-ASProtocolVersions` list is
/// ASSUMED ascending, so take the LAST client-known entry in the server's
/// listed order — deliberately NO numeric sort (an unsorted server list is
/// honoured as-is). Entries are whitespace-trimmed. `None` when no server
/// version is in `client_known`.
pub fn pick_protocol_version(server_list: &str, client_known: &[&str]) -> Option<String> {
    server_list
        .split(',')
        .map(str::trim)
        .rfind(|v| !v.is_empty() && client_known.contains(v))
        .map(|v| v.to_string())
}

/// Pure half of `EasClient::options()`: split the two MS-ASHTTP OPTIONS
/// response headers into an `EasServerOptions`. Both headers absent →
/// `EasError::Transport` (not an EAS endpoint); one absent → empty list on
/// that side. Pure / no I/O so it is unit-testable without a live socket.
fn parse_options_headers(
    versions: Option<&str>,
    commands: Option<&str>,
) -> Result<EasServerOptions, EasError> {
    if versions.is_none() && commands.is_none() {
        return Err(EasError::Transport(
            "OPTIONS response carried neither MS-ASProtocolVersions nor MS-ASProtocolCommands"
                .into(),
        ));
    }
    let split = |s: Option<&str>| -> Vec<String> {
        s.unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect()
    };
    Ok(EasServerOptions {
        protocol_versions: split(versions),
        commands: split(commands),
    })
}

/// Hex-dump helper for wire-level request/response logs, capped so a large
/// Sync body can't flood the log file (Ping bodies are ~20-120 bytes).
fn hex_capped(bytes: &[u8], cap: usize) -> String {
    let n = bytes.len().min(cap);
    let mut s = bytes[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if bytes.len() > cap {
        s.push_str(&format!("…(+{}B)", bytes.len() - cap));
    }
    s
}

/// Whether the wire-level body dumps in `send_command_no_retry` (the DEBUG
/// request/response dumps AND the WARN parse-failure preview) may print a
/// command's raw WBXML bytes. Three commands carry content that must never
/// reach the log at ANY level:
///   - `Settings` — the DevicePassword Set form transports the device's recovery password, and the
///     Oof forms transport (Set) / return (Get) the user's auto-reply messages;
///   - `Provision` — the device-security policy exchange;
///   - `ValidateCert` — the request carries certificate payloads: opaque base64 DER blobs that are
///     large and security-sensitive material;
///   - `ResolveRecipients` — the request carries directory lookup strings and the response carries
///     directory PII (names, SMTP addresses) plus free/busy data.
///
/// Redaction is applied at the dump call sites, NOT inside the WBXML body
/// (which would be invasive and schema-dependent): redacted commands log a
/// `<redacted:Cmd>` placeholder with the byte count instead, so a debug
/// session still sees that a body went out and how large it was — just not
/// what it says. Release builds run at Info, so this gate only ever affects
/// DEBUG-level logs — which is exactly where the plaintext was leaking.
/// Pure / no I/O — unit-tested without a transport.
fn body_dump_allowed(command: &str) -> bool {
    !matches!(
        command,
        "Provision" | "Settings" | "ValidateCert" | "ResolveRecipients"
    )
}

/// Body preview for the WBXML parse-failure warn (`deserialize_to_tree`
/// error path in `send_command_no_retry`). That warn fires at WARN level —
/// which RELEASE builds emit — so it is gated by the same
/// [`body_dump_allowed`] decision as the DEBUG dumps: a malformed Settings
/// Oof Get response still carries the user's reply text in its leading
/// bytes, and a parse failure is exactly when this error path fires.
/// Secret-bearing commands get the `<redacted:Cmd>` placeholder (the byte
/// count and the parse error are still logged — only the raw bytes are
/// suppressed); every other command keeps the pre-existing first-64-bytes
/// uppercase-hex preview. Pure / no I/O — unit-tested without a transport.
fn parse_failure_preview(body: &[u8], command: &str) -> String {
    if body_dump_allowed(command) {
        format!("{:02X?}", &body[..body.len().min(64)])
    } else {
        format!("<redacted:{command}>")
    }
}

/// How a 200-OK response body must be interpreted, decided from the
/// Content-Type header and the request's multipart opt-in. Pure / no I/O —
/// unit-tested without a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseBranch {
    /// `application/vnd.ms-sync.multipart` ([MS-ASCMD] §2.2.1.10.1).
    Multipart,
    /// `application/vnd.ms-sync.wbxml` — the default inline form.
    Wbxml,
    /// Anything else (HTML error page, OWA login, missing header). The
    /// caller builds the 200-byte-preview Transport error for this arm.
    Unexpected,
}

/// Classify the response Content-Type. Multipart is accepted ONLY when the
/// request carried `MS-ASAcceptMultiPart: T` (`accept_multipart`) — the
/// spec permits a multipart response solely in reply to an opted-in
/// request, so an unrequested one is a protocol violation: an Err here
/// (the caller additionally warn-logs) rather than a silent parse of a
/// body shape we never asked for.
fn response_branch(content_type: &str, accept_multipart: bool) -> Result<ResponseBranch, EasError> {
    if content_type.contains("vnd.ms-sync.multipart") {
        if accept_multipart {
            Ok(ResponseBranch::Multipart)
        } else {
            Err(EasError::Transport(format!(
                "server returned multipart content-type '{content_type}' but the request did not carry MS-ASAcceptMultiPart: T"
            )))
        }
    } else if content_type.contains("vnd.ms-sync.wbxml") {
        Ok(ResponseBranch::Wbxml)
    } else {
        Ok(ResponseBranch::Unexpected)
    }
}

/// Minimal form-urlencoder for the handful of query string values we emit.
/// Avoids pulling in a `urlencoding` crate dependency.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
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
fn parse_retry_after_delta(header_value: &str, now_epoch: i64) -> Option<i64> {
    let delta: i64 = header_value.trim().parse().ok()?;
    Some(now_epoch + delta)
}

/// The fixed EAS endpoint path ([MS-ASHTTP] §2.1). Redirect adoption derives
/// the new endpoint as `https://<location authority>` + this path — the
/// location's own path/query/fragment never carries over (so a location that
/// already ends in the EAS path is never doubled).
const EAS_ENDPOINT_PATH: &str = "/Microsoft-Server-ActiveSync";

/// Validate an HTTP 451 `X-MS-Location` header value ([MS-ASHTTP]
/// §2.2.1.1.2.4) and derive the EAS endpoint URL to adopt:
///   * must be an ABSOLUTE `https://` URL — an `http://` location is a plaintext downgrade and
///     anything else (relative, other scheme, garbage) is untrusted; both reject with a descriptive
///     error, never silently;
///   * must carry a host;
///   * the authority must carry NO userinfo (`user[:password]@host`): the server-controlled,
///     credential-shaped string would otherwise be persisted verbatim into the plaintext
///     `accounts.eas_url` column and the hop log. Rejected with a descriptive error — never
///     silently stripped and adopted — matching the refuse-downgrade posture; the userinfo value
///     itself is never logged (only the host is);
///   * the new endpoint is `https://` + the location's authority (host and port, case preserved) +
///     the fixed [`EAS_ENDPOINT_PATH`] — any path/query/fragment the location carries is dropped,
///     so a location already ending in the EAS path is never doubled and any query string is
///     stripped.
///
/// Scheme match is case-insensitive per RFC 3986 (`HTTPS://` is accepted).
/// No network I/O — unit-tested without a live server (a rejection only
/// emits a redacted warn log).
fn endpoint_from_x_ms_location(location: &str) -> Result<String, EasError> {
    const HTTPS_SCHEME: &str = "https://";
    let trimmed = location.trim();
    let lower = trimmed.to_ascii_lowercase();
    let after_scheme = if lower.starts_with(HTTPS_SCHEME) {
        // Re-slice the ORIGINAL value at the same byte offset — ASCII case
        // folding preserves length, and the authority's case is kept as the
        // server sent it.
        &trimmed[HTTPS_SCHEME.len()..]
    } else if lower.starts_with("http://") {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' is a plaintext http:// URL — refusing to downgrade from https"
        )));
    } else {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' is not an absolute https:// URL — refusing to follow"
        )));
    };
    // The authority ends at the first '/', '?' or '#' (path/query/fragment).
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.is_empty() {
        return Err(EasError::Transport(format!(
            "X-MS-Location rejected: '{trimmed}' carries no host — refusing to follow"
        )));
    }
    // The authority must carry NO userinfo (`user[:password]@host`): the
    // server-controlled, credential-shaped string would otherwise land
    // verbatim in the hop log and the plaintext `accounts.eas_url` column.
    // Reject — never strip-and-adopt — matching the refuse-downgrade
    // posture. Per RFC 3986 the LAST '@' delimits userinfo from host, so
    // the remainder is the (non-credential) host[:port] — safe to log; the
    // userinfo value itself never goes into the log or the error.
    if authority.contains('@') {
        let host = authority.rsplit('@').next().unwrap_or_default();
        log::warn!(
            "EAS HTTP 451 redirect rejected: X-MS-Location authority carries userinfo (redirect host: {host}) — refusing to follow; the credential-shaped userinfo is deliberately not logged or persisted"
        );
        return Err(EasError::Transport(
            "X-MS-Location rejected: the authority carries userinfo (a credential-shaped prefix before the host) — refusing to follow; the userinfo value is deliberately not logged or persisted"
                .to_string(),
        ));
    }
    Ok(format!("https://{authority}{EAS_ENDPOINT_PATH}"))
}

/// Per-request timeout for a Ping: the requested heartbeat plus a 60s margin
/// for response delivery/parse time. The client-wide reqwest default is 120s
/// (`EasClient::new`), which otherwise kills every server-held ping at 120s
/// while the tuned heartbeat can reach 480s (cap 1680s) — reqwest's
/// per-request `RequestBuilder::timeout` overrides that default. Saturating:
/// a u32::MAX heartbeat must not overflow the margin add.
fn ping_request_timeout(heartbeat_secs: u32) -> std::time::Duration {
    std::time::Duration::from_secs(heartbeat_secs.saturating_add(60) as u64)
}

/// MS-ASPing: status 5 = requested heartbeat out of range; the response's
/// HeartbeatInterval carries a supported value → retry once with it.
fn ping_retry_interval(status: &str, server_interval: Option<u32>) -> Option<u32> {
    if status == "5" { server_interval } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_alphanumeric() {
        assert_eq!(urlencode("abcXYZ123"), "abcXYZ123");
    }

    #[test]
    fn urlencode_escapes_special() {
        assert_eq!(urlencode("user@host"), "user%40host");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("kylins\\admin"), "kylins%5Cadmin");
    }

    #[test]
    fn urlencode_keeps_unreserved() {
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencode_empty() {
        assert_eq!(urlencode(""), "");
    }

    #[test]
    fn hierarchy_key_falls_back_to_zero_before_first_folder_sync() {
        let client = EasClient::new(EasConfig::default());
        assert_eq!(client.hierarchy_key(), "0");
    }

    /// Phase B Task 2: the public getter surfaces the in-memory cached key so
    /// `EasSource::list_folders` can persist it after a successful FolderSync.
    /// Pre-FolderSync it yields the same "0" fallback as `hierarchy_key()`.
    #[test]
    fn hierarchy_sync_key_str_returns_cached_key_or_zero_fallback() {
        let mut client = EasClient::new(EasConfig::default());
        assert_eq!(
            client.hierarchy_sync_key_str(),
            "0",
            "empty cache must surface the \"0\" bootstrap fallback"
        );
        client.hierarchy_sync_key = "hier-9".to_string(); // as if FolderSync ran
        assert_eq!(client.hierarchy_sync_key_str(), "hier-9");
    }

    /// Phase B Task 7: `set_hierarchy_sync_key` primes the cache from the
    /// persisted key so a folder op can go out without a preceding FolderSync;
    /// an empty string is ignored (the "0" bootstrap fallback must survive).
    #[test]
    fn set_hierarchy_sync_key_primes_cache_and_ignores_empty() {
        let mut client = EasClient::new(EasConfig::default());
        client.set_hierarchy_sync_key(String::new());
        assert_eq!(client.hierarchy_key(), "0", "empty prime must be a no-op");
        client.set_hierarchy_sync_key("hier-7".to_string());
        assert_eq!(client.hierarchy_key(), "hier-7");
    }

    /// [MS-ASCMD] 2.2.3.181.1: a folder-op response carries the NEW hierarchy
    /// SyncKey; the client must adopt it or the next folder op goes out stale.
    #[test]
    fn folder_op_response_sync_key_is_adopted() {
        let mut client = EasClient::new(EasConfig::default());
        client.hierarchy_sync_key = "1".to_string(); // as if FolderSync ran
        let resp = WbxmlElement::container(
            PAGE_FOLDER,
            FH_FOLDER_CREATE,
            vec![
                WbxmlElement::text(PAGE_FOLDER, 0x0C, "1"),  // Status
                WbxmlElement::text(PAGE_FOLDER, 0x12, "2"),  // SyncKey
                WbxmlElement::text(PAGE_FOLDER, 0x08, "52"), // ServerId
            ],
        );
        client.adopt_folder_op_sync_key(&resp);
        assert_eq!(client.hierarchy_key(), "2");
    }

    /// Error responses may omit SyncKey — the cached key must survive.
    #[test]
    fn folder_op_response_without_sync_key_keeps_existing_key() {
        let mut client = EasClient::new(EasConfig::default());
        client.hierarchy_sync_key = "1".to_string();
        let resp = WbxmlElement::container(
            PAGE_FOLDER,
            FH_FOLDER_CREATE,
            vec![WbxmlElement::text(PAGE_FOLDER, 0x0C, "10")], // Status only
        );
        client.adopt_folder_op_sync_key(&resp);
        assert_eq!(client.hierarchy_key(), "1");
    }

    // ---- Sync empty body = no changes (hotfix 2026-08-03) ----
    //
    // Exchange returns an EMPTY HTTP body for Sync when the collection has
    // nothing to report (Android EasSync.java:225 treats empty as OK).
    // `no_changes_result` is the pure decision: build the no-changes
    // SyncResult that PRESERVES the request's sync key (Default's empty key
    // would corrupt the engine's cursor).

    #[test]
    fn no_changes_result_preserves_request_key() {
        let r = no_changes_result("sync-key-42");
        assert_eq!(r.sync_key, "sync-key-42");
    }

    #[test]
    fn no_changes_result_is_success_with_no_items() {
        let r = no_changes_result("k");
        assert_eq!(r.status, 1, "no-changes must read as success");
        assert!(r.added.is_empty());
        assert!(r.updated.is_empty());
        assert!(r.deleted_server_ids.is_empty());
        assert!(!r.more_available);
    }

    #[test]
    fn no_changes_result_empty_key_stays_empty() {
        let r = no_changes_result("");
        assert_eq!(r.sync_key, "");
        assert_eq!(r.status, 1);
    }

    // ---- Ping per-request timeout (hotfix 2026-08-03) ----
    //
    // `EasClient::new` sets a client-wide 120s reqwest timeout, but the tuned
    // ping heartbeat can reach 480s (cap 1680s) — every server-held ping died
    // client-side at exactly 120s as a transport failure, causing strikes and
    // a drop to poll. Ping now passes a per-request timeout of heartbeat + 60s
    // margin (reqwest's `RequestBuilder::timeout` overrides the client
    // default); every other command keeps the global default.

    #[test]
    fn ping_request_timeout_is_heartbeat_plus_margin() {
        // Normal tuned heartbeat: 480s + 60s margin.
        assert_eq!(
            ping_request_timeout(480),
            std::time::Duration::from_secs(540)
        );
    }

    #[test]
    fn ping_request_timeout_zero_heartbeat_is_margin_only() {
        assert_eq!(ping_request_timeout(0), std::time::Duration::from_secs(60));
    }

    #[test]
    fn ping_request_timeout_saturates_at_u32_max() {
        // u32::MAX + 60 must not overflow — saturating_add keeps u32::MAX.
        assert_eq!(
            ping_request_timeout(u32::MAX),
            std::time::Duration::from_secs(u32::MAX as u64)
        );
    }

    // ---- F2: Ping heartbeat-interval retry DECISION ----

    #[test]
    fn ping_retry_interval_only_on_status_5_with_value() {
        assert_eq!(ping_retry_interval("5", Some(60)), Some(60));
        assert_eq!(ping_retry_interval("5", None), None);
        assert_eq!(ping_retry_interval("2", Some(60)), None);
        assert_eq!(ping_retry_interval("1", None), None);
    }

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

    // ---- F4: SendMail family — empty body is success ----

    #[test]
    fn empty_body_allowed_for_compose_commands() {
        assert!(empty_body_outcome(true).is_none());
        assert!(matches!(
            empty_body_outcome(false),
            Some(EasError::Transport(_))
        ));
    }

    // ---- Task 9: SmartForward → SendMail degradation DECISION ----
    //
    // `should_degrade_to_send_mail` is the pure decision function
    // `smart_forward` consults when the SmartForward command fails. The
    // fallback path itself needs a live server (Task 10's probe); the
    // decision logic is unit-testable.

    /// A command-level rejection (any status — e.g. original attachments
    /// gone, source item not found) means the server cannot honour the
    /// SmartForward semantics → degrade to plain SendMail.
    #[test]
    fn should_degrade_on_command_status() {
        let err = EasError::CommandStatus {
            status: 150,
            message: "item not found".into(),
        };
        assert!(should_degrade_to_send_mail(&err));
        // Any status qualifies, not a curated subset.
        let err = EasError::CommandStatus {
            status: 110,
            message: "server error".into(),
        };
        assert!(should_degrade_to_send_mail(&err));
    }

    /// Transport / HTTP / WBXML errors are NOT command rejections — the
    /// SmartForward may still succeed on retry, so they surface unchanged.
    #[test]
    fn should_not_degrade_on_non_command_errors() {
        assert!(!should_degrade_to_send_mail(&EasError::Transport(
            "socket reset".into()
        )));
        assert!(!should_degrade_to_send_mail(&EasError::HttpStatus {
            status: 503,
            body: "busy".into(),
            retry_after: None,
            x_ms_location: None,
        }));
        assert!(!should_degrade_to_send_mail(&EasError::Wbxml(
            WbxmlError::UnexpectedEof
        )));
    }

    /// EAS compose rejections commonly arrive as HTTP 200 + in-body
    /// `<Status>` (parse_send_mail_response → Ok(status != 1)) rather than
    /// an Err — those must ALSO degrade to SendMail. Status 1 = success.
    #[test]
    fn should_degrade_on_inbody_status_not_success() {
        assert!(smart_forward_inbody_status_failed(110));
        assert!(smart_forward_inbody_status_failed(150));
        assert!(!smart_forward_inbody_status_failed(1));
    }

    // ---- Phase B Task 3: Options + version negotiation ----
    //
    // `pick_protocol_version` ports Android's EasOptions algorithm: the
    // server's MS-ASProtocolVersions list is ASSUMED ascending — take the
    // LAST client-known entry in the server's listed order, never a
    // numeric sort. `parse_options_headers` is the pure half of
    // `EasClient::options()` (header-map extraction is reqwest's job).

    #[test]
    fn pick_protocol_version_picks_last_known_in_server_order() {
        let known = ["16.0", "16.1"];
        assert_eq!(
            pick_protocol_version("2.5,12.1,14.0,14.1,16.0,16.1", &known),
            Some("16.1".to_string())
        );
    }

    #[test]
    fn pick_protocol_version_unsorted_server_list_keeps_server_order() {
        // No numeric sort: the LAST known entry in the listed order wins,
        // even when the server lists them descending.
        let known = ["14.0", "16.1"];
        assert_eq!(
            pick_protocol_version("16.1,14.0", &known),
            Some("14.0".to_string())
        );
    }

    #[test]
    fn pick_protocol_version_no_match_returns_none() {
        let known = ["99.9"];
        assert_eq!(pick_protocol_version("2.5,12.1,16.1", &known), None);
    }

    #[test]
    fn pick_protocol_version_empty_inputs_return_none() {
        let known = ["16.1"];
        assert_eq!(pick_protocol_version("", &known), None);
        let empty: [&str; 0] = [];
        assert_eq!(pick_protocol_version("16.1", &empty), None);
    }

    #[test]
    fn pick_protocol_version_tolerates_whitespace_around_entries() {
        let known = ["16.1"];
        assert_eq!(
            pick_protocol_version(" 2.5 , 14.0 , 16.1 ", &known),
            Some("16.1".to_string())
        );
    }

    #[test]
    fn parse_options_headers_splits_and_trims_both_lists() {
        let opts = parse_options_headers(
            Some("2.5,12.0,12.1,14.0,14.1,16.0,16.1"),
            Some("Sync,SendMail,Provision, FolderSync"),
        )
        .expect("both headers present");
        assert_eq!(
            opts.protocol_versions,
            vec!["2.5", "12.0", "12.1", "14.0", "14.1", "16.0", "16.1"]
        );
        assert_eq!(
            opts.commands,
            vec!["Sync", "SendMail", "Provision", "FolderSync"]
        );
    }

    #[test]
    fn parse_options_headers_missing_both_is_transport_error() {
        assert!(matches!(
            parse_options_headers(None, None),
            Err(EasError::Transport(_))
        ));
    }

    #[test]
    fn parse_options_headers_one_missing_yields_empty_list() {
        let opts = parse_options_headers(Some("16.0,16.1"), None).expect("versions only");
        assert_eq!(opts.protocol_versions, vec!["16.0", "16.1"]);
        assert!(opts.commands.is_empty());

        let opts = parse_options_headers(None, Some("Sync")).expect("commands only");
        assert!(opts.protocol_versions.is_empty());
        assert_eq!(opts.commands, vec!["Sync"]);
    }

    // ---- Task 3: HTTP 451 X-MS-Location redirect follow ----
    //
    // [MS-ASHTTP] §2.2.1.1.2.4 / §3.1.5.2: an HTTP 451 response carries an
    // X-MS-Location header with the full URL of the new server; the client
    // adopts it and re-issues the command. `endpoint_from_x_ms_location` is
    // the pure validation/derivation helper and `adopt_redirect_location` the
    // client-side adoption step — both unit-testable without a live server
    // (the retry-loop wiring itself needs one; the validation + adoption
    // halves are the load-bearing logic and are covered here).

    #[test]
    fn x_ms_location_https_location_yields_fixed_eas_endpoint() {
        // Full EAS URL form (the common shape per [MS-ASHTTP]).
        assert_eq!(
            endpoint_from_x_ms_location("https://mail.contoso.com/Microsoft-Server-ActiveSync")
                .expect("valid https location must derive an endpoint"),
            "https://mail.contoso.com/Microsoft-Server-ActiveSync"
        );
        // Bare host: the fixed EAS path is appended.
        assert_eq!(
            endpoint_from_x_ms_location("https://newhost.example.com")
                .expect("valid https location must derive an endpoint"),
            "https://newhost.example.com/Microsoft-Server-ActiveSync"
        );
        // Port is preserved; a foreign path is replaced by the fixed EAS path.
        assert_eq!(
            endpoint_from_x_ms_location("https://mail.contoso.com:8443/some/other/path")
                .expect("valid https location must derive an endpoint"),
            "https://mail.contoso.com:8443/Microsoft-Server-ActiveSync"
        );
    }

    /// A location that already ends in the EAS path must not get the path
    /// doubled; the derived endpoint is always scheme + authority + the fixed
    /// path, so any path the location carries is normalized away.
    #[test]
    fn x_ms_location_eas_path_in_location_is_not_doubled() {
        assert_eq!(
            endpoint_from_x_ms_location("https://new.example.com/Microsoft-Server-ActiveSync")
                .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(
            endpoint_from_x_ms_location("https://new.example.com/Microsoft-Server-ActiveSync/")
                .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
    }

    /// Any query string the location carries is stripped — the command query
    /// string (Cmd/User/DeviceId/DeviceType) is rebuilt per request.
    #[test]
    fn x_ms_location_query_string_is_stripped() {
        assert_eq!(
            endpoint_from_x_ms_location(
                "https://new.example.com/Microsoft-Server-ActiveSync?originalReq=abc&x=1"
            )
            .expect("valid https location must derive an endpoint"),
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
    }

    /// Following an `http://` location would downgrade the connection to
    /// plaintext — rejected with a descriptive error, never silently adopted.
    #[test]
    fn x_ms_location_http_downgrade_is_rejected() {
        let err =
            endpoint_from_x_ms_location("http://mail.contoso.com/Microsoft-Server-ActiveSync")
                .expect_err("plaintext http:// location must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("http://") && msg.to_ascii_lowercase().contains("refus"),
            "error must describe the refused downgrade, got: {msg}"
        );
    }

    /// Unparseable / relative / wrong-scheme values are rejected — never
    /// silently kept on the old URL and never trusted as-is.
    #[test]
    fn x_ms_location_garbage_is_rejected() {
        for bad in [
            "",
            "not a url",
            "Microsoft-Server-ActiveSync", // relative
            "ftp://mail.contoso.com/",     // wrong scheme
            "//mail.contoso.com/Microsoft-Server-ActiveSync", // scheme-relative
        ] {
            assert!(
                endpoint_from_x_ms_location(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    /// An https URL without a host cannot become an endpoint.
    #[test]
    fn x_ms_location_hostless_url_is_rejected() {
        for bad in [
            "https://",
            "https:///?x=1",
            "https:///Microsoft-Server-ActiveSync",
        ] {
            assert!(
                endpoint_from_x_ms_location(bad).is_err(),
                "'{bad}' must be rejected (no host)"
            );
        }
    }

    /// A location carrying userinfo (`user:pass@host`, or `user@host` with no
    /// password) is REJECTED, never adopted: the credential-shaped,
    /// server-controlled string would otherwise land verbatim in the hop log
    /// and the plaintext `accounts.eas_url` column. Rejection (not silent
    /// stripping) matches the refuse-http-downgrade posture. The error must
    /// name the problem WITHOUT echoing the credential-shaped location.
    #[test]
    fn x_ms_location_userinfo_is_rejected() {
        for bad in [
            "https://user:pass@mail.example.com/Microsoft-Server-ActiveSync",
            "https://user@mail.example.com/Microsoft-Server-ActiveSync", // user, no password
            "https://user:pass@mail.example.com",                        // bare authority
            "https://user:pass@mail.example.com:8443/",                  // userinfo + port
        ] {
            let err = endpoint_from_x_ms_location(bad)
                .expect_err("location with userinfo in the authority must be rejected");
            let msg = err.to_string();
            let lower = msg.to_ascii_lowercase();
            assert!(
                lower.contains("userinfo") || lower.contains("credential"),
                "error must name the userinfo/credential problem for '{bad}', got: {msg}"
            );
            assert!(
                !msg.contains("user:pass") && !msg.contains("user@"),
                "error must not echo the credential-shaped location for '{bad}', got: {msg}"
            );
        }
    }

    /// RFC 3986: the scheme is case-insensitive. The authority's case is
    /// preserved (hostnames are case-insensitive, but we don't rewrite what
    /// the server sent).
    #[test]
    fn x_ms_location_scheme_case_insensitive_authority_preserved() {
        assert_eq!(
            endpoint_from_x_ms_location("HTTPS://Mail.Contoso.COM/owa")
                .expect("valid https location must derive an endpoint"),
            "https://Mail.Contoso.COM/Microsoft-Server-ActiveSync"
        );
    }

    /// Adopting a valid location switches the client's base URL and records
    /// the adopted target for the source layer to persist.
    #[test]
    fn adopt_redirect_location_switches_base_url_and_records_it() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        assert_eq!(client.adopted_url(), None);
        client
            .adopt_redirect_location("https://new.example.com/Microsoft-Server-ActiveSync")
            .expect("valid https location must be adopted");
        assert_eq!(
            client.config.url,
            "https://new.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(
            client.adopted_url(),
            Some("https://new.example.com/Microsoft-Server-ActiveSync")
        );
    }

    /// An invalid location must NOT switch the base URL and must NOT record
    /// an adoption — the error surfaces to the caller.
    #[test]
    fn adopt_redirect_location_invalid_keeps_old_url() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        assert!(
            client
                .adopt_redirect_location("http://evil.example.com/Microsoft-Server-ActiveSync")
                .is_err()
        );
        assert!(client.adopt_redirect_location("garbage").is_err());
        assert_eq!(
            client.config.url,
            "https://old.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(client.adopted_url(), None);
    }

    /// A userinfo location must NOT switch the base URL and must NOT record
    /// an adoption — the old URL stays untouched, nothing credential-shaped
    /// is persisted or recorded, and the error surfaces to the caller.
    #[test]
    fn adopt_redirect_location_userinfo_keeps_old_url() {
        let mut client = EasClient::new(EasConfig {
            url: "https://old.example.com/Microsoft-Server-ActiveSync".into(),
            ..EasConfig::default()
        });
        for bad in [
            "https://user:pass@new.example.com/Microsoft-Server-ActiveSync",
            "https://user@new.example.com/Microsoft-Server-ActiveSync", // user, no password
        ] {
            assert!(
                client.adopt_redirect_location(bad).is_err(),
                "'{bad}' must be rejected (userinfo in authority)"
            );
        }
        assert_eq!(
            client.config.url,
            "https://old.example.com/Microsoft-Server-ActiveSync"
        );
        assert_eq!(client.adopted_url(), None);
    }

    // The per-hop decision of the redirect follow loop ([MS-ASHTTP] §3.1.5.2)
    // is a pure boundary: follow while hops < MAX_REDIRECT_HOPS and a
    // location is present; surface the 451 at the cap or without a location.
    // The loop wiring needs a live server, but the boundary itself is
    // unit-testable.

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

    // ---- Command size-limit chunking ([MS-ASCMD] §3.1.5.10) ----
    //
    // Pure chunk-boundary tests: `command_chunks` is the boundary both
    // `move_items` (≤MOVE_ITEMS_MAX_PER_COMMAND Move elements per request)
    // and `sync_changes` (≤SYNC_MAX_COMMANDS_PER_REQUEST Sync command
    // elements per request) split through before sending sequential wire
    // commands. The send loops themselves need a live server; the boundary
    // shapes and the input-ordering guarantee are unit-testable without one
    // (same split as the `redirect_hop_decision` precedent above).

    fn move_tuple(i: usize) -> (String, String, String) {
        (
            format!("msg:{i}"),
            "srcfld".to_string(),
            "dstfld".to_string(),
        )
    }

    fn flag_change(i: usize) -> commands::EasChange {
        commands::EasChange {
            server_id: format!("srv:{i}"),
            read: Some(true),
            starred: None,
        }
    }

    /// Spec constant guard: these ARE the [MS-ASCMD] §3.1.5.10 SHOULD-limits
    /// (MoveItems Move elements = 1000; Sync Add+Change+Delete+Fetch elements
    /// = 200). Changing either requires re-verifying against the spec table.
    #[test]
    fn size_limit_constants_match_msascmd_3_1_5_10() {
        assert_eq!(MOVE_ITEMS_MAX_PER_COMMAND, 1000);
        assert_eq!(SYNC_MAX_COMMANDS_PER_REQUEST, 200);
    }

    #[test]
    fn move_items_exactly_at_limit_is_one_chunk() {
        let moves: Vec<_> = (0..MOVE_ITEMS_MAX_PER_COMMAND).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(
            chunks.len(),
            1,
            "exactly 1000 moves fit in a single MoveItems command"
        );
        assert_eq!(chunks[0].len(), MOVE_ITEMS_MAX_PER_COMMAND);
    }

    #[test]
    fn move_items_one_over_limit_splits_1000_plus_1() {
        let moves: Vec<_> = (0..MOVE_ITEMS_MAX_PER_COMMAND + 1)
            .map(move_tuple)
            .collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 2, "1001 moves must split into two commands");
        assert_eq!(chunks[0].len(), MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn move_items_2500_splits_1000_1000_500() {
        let moves: Vec<_> = (0..2500).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 1000);
        assert_eq!(chunks[1].len(), 1000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn move_items_empty_input_is_zero_chunks() {
        let moves: Vec<(String, String, String)> = Vec::new();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert!(chunks.is_empty(), "no moves → no MoveItems commands");
    }

    #[test]
    fn sync_changes_exactly_at_limit_is_one_chunk() {
        let changes: Vec<_> = (0..SYNC_MAX_COMMANDS_PER_REQUEST)
            .map(flag_change)
            .collect();
        let chunks = command_chunks(&changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(
            chunks.len(),
            1,
            "exactly 200 changes fit in a single Sync command"
        );
        assert_eq!(chunks[0].len(), SYNC_MAX_COMMANDS_PER_REQUEST);
    }

    #[test]
    fn sync_changes_one_over_limit_splits_200_plus_1() {
        let changes: Vec<_> = (0..SYNC_MAX_COMMANDS_PER_REQUEST + 1)
            .map(flag_change)
            .collect();
        let chunks = command_chunks(&changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(chunks.len(), 2, "201 changes must split into two commands");
        assert_eq!(chunks[0].len(), SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(chunks[1].len(), 1);
    }

    /// The result-merge ordering guarantee: chunks cover the input
    /// contiguously in request order, so merging per-chunk results
    /// chunk-by-chunk keeps the merged `(Status, DstMsgId)` pairs aligned
    /// with the request tuples (move_items' merge contract).
    #[test]
    fn command_chunks_preserves_input_order_and_coverage() {
        let moves: Vec<_> = (0..3001).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 4, "3001 moves = 1000+1000+1000+1");
        let flattened: Vec<&(String, String, String)> =
            chunks.iter().flat_map(|c| c.iter()).collect();
        assert_eq!(flattened.len(), moves.len());
        for (i, m) in flattened.iter().enumerate() {
            assert_eq!(m.0, format!("msg:{i}"), "input order broken at index {i}");
        }
    }

    // ---- DEBUG wire-level body-dump redaction gate ----
    //
    // `send_command_no_retry` hex-dumps WBXML bodies at DEBUG level. Three
    // commands carry content that must never reach the log at ANY level:
    // Settings (DevicePassword recovery passwords, OOF auto-reply messages
    // in both request and Get response), Provision (the device-security
    // policy exchange), and ValidateCert (certificate payloads — large,
    // security-sensitive base64 DER material). `body_dump_allowed` is the
    // pure redaction decision those dump sites consult.

    #[test]
    fn body_dump_redacts_secret_bearing_commands() {
        assert!(
            !body_dump_allowed("Settings"),
            "Settings carries DevicePassword passwords and OOF reply messages"
        );
        assert!(
            !body_dump_allowed("Provision"),
            "Provision is the device-security policy exchange"
        );
        assert!(
            !body_dump_allowed("ValidateCert"),
            "ValidateCert carries certificate payloads (security-sensitive material)"
        );
        assert!(
            !body_dump_allowed("ResolveRecipients"),
            "ResolveRecipients carries directory lookup strings + PII (names, SMTP addresses, free/busy)"
        );
    }

    #[test]
    fn body_dump_stays_enabled_for_non_secret_commands() {
        for cmd in [
            "Sync",
            "FolderSync",
            "Ping",
            "SendMail",
            "SmartForward",
            "SmartReply",
            "ItemOperations",
            "GetItemEstimate",
            "MeetingResponse",
            "Search",
            "MoveItems",
        ] {
            assert!(
                body_dump_allowed(cmd),
                "body dump must stay enabled for {cmd}"
            );
        }
    }

    /// The WBXML parse-failure warn fires at WARN level — emitted in RELEASE
    /// builds — so it must not dump the first 64 bytes of a secret-bearing
    /// response (a malformed Settings Oof Get response still carries reply
    /// text in its leading bytes). `parse_failure_preview` reuses the
    /// `body_dump_allowed` gate: placeholder for secret-bearing commands.
    #[test]
    fn parse_failure_preview_redacts_secret_bearing_commands() {
        let body: Vec<u8> = (0u8..100).collect();
        assert_eq!(
            parse_failure_preview(&body, "Settings"),
            "<redacted:Settings>"
        );
        assert_eq!(
            parse_failure_preview(&body, "Provision"),
            "<redacted:Provision>"
        );
        assert_eq!(
            parse_failure_preview(&body, "ResolveRecipients"),
            "<redacted:ResolveRecipients>"
        );
    }

    /// Non-secret commands keep the exact pre-existing behavior: the first
    /// 64 bytes as `{:02X?}` uppercase hex, truncated (not padded) at 64.
    #[test]
    fn parse_failure_preview_keeps_hex_preview_for_non_secret_commands() {
        let body: Vec<u8> = vec![0x03, 0x01, 0x6A, 0x00];
        assert_eq!(parse_failure_preview(&body, "Sync"), "[03, 01, 6A, 00]");
        // Bodies longer than 64 bytes truncate at 64 (the pre-existing
        // `&body[..body.len().min(64)]` behavior).
        let long: Vec<u8> = (0u8..100).collect();
        let preview = parse_failure_preview(&long, "Sync");
        assert!(preview.starts_with("[00, 01, 02,"));
        assert!(
            preview.ends_with("3F]"),
            "preview must stop at byte 64: {preview}"
        );
        assert!(
            !preview.contains("40"),
            "byte 65+ must not appear: {preview}"
        );
    }

    /// ResolveRecipients with an EMPTY To list is rejected client-side
    /// before any network I/O: §2.2.3.191 requires at least one To, and an
    /// empty request is pointless (nothing to resolve). The error names the
    /// command so the caller's log alone diagnoses the misuse.
    #[tokio::test]
    async fn resolve_recipients_rejects_empty_to_list() {
        let mut client = EasClient::new(EasConfig::default());
        let req = ResolveRecipientsRequest {
            to: vec![],
            max_ambiguous_recipients: Some(5),
            availability: None,
        };
        let err = client
            .resolve_recipients(&req)
            .await
            .expect_err("empty To list must be rejected before any network I/O");
        let msg = err.to_string();
        assert!(
            msg.contains("ResolveRecipients"),
            "error must name the command: {msg}"
        );
        assert!(
            msg.contains("To"),
            "error must name the rejected field: {msg}"
        );
    }

    // ---- multipart response Content-Type branching ([MS-ASCMD] §2.2.1.10.1) ----

    #[test]
    fn response_branch_wbxml_content_type() {
        let branch = response_branch("application/vnd.ms-sync.wbxml", false)
            .expect("wbxml is always accepted");
        assert_eq!(branch, ResponseBranch::Wbxml);
        // Opting in to multipart must not break plain WBXML responses —
        // the server MAY ignore MS-ASAcceptMultiPart and answer inline.
        let branch = response_branch("application/vnd.ms-sync.wbxml", true)
            .expect("wbxml stays accepted when opted in");
        assert_eq!(branch, ResponseBranch::Wbxml);
    }

    #[test]
    fn response_branch_multipart_when_opted_in() {
        let branch = response_branch("application/vnd.ms-sync.multipart", true)
            .expect("multipart is accepted when the request opted in");
        assert_eq!(branch, ResponseBranch::Multipart);
    }

    #[test]
    fn response_branch_multipart_without_opt_in_is_protocol_violation() {
        // [MS-ASCMD] §2.2.1.10.1: a server may only send a multipart response
        // to a request carrying MS-ASAcceptMultiPart: T. Multipart WITHOUT
        // the opt-in is a protocol violation — never silently parse it.
        let err = response_branch("application/vnd.ms-sync.multipart", false)
            .expect_err("unrequested multipart must error");
        let msg = err.to_string();
        assert!(
            msg.contains("MS-ASAcceptMultiPart"),
            "error must name the missing opt-in header: {msg}"
        );
    }

    #[test]
    fn response_branch_unexpected_content_type() {
        // HTML error page / OWA login: the caller builds the 200-byte
        // preview error for this arm.
        let branch = response_branch("text/html; charset=utf-8", false)
            .expect("unexpected types classify, they do not error here");
        assert_eq!(branch, ResponseBranch::Unexpected);
        // Empty Content-Type header (missing) classifies as Unexpected too.
        let branch = response_branch("", true).expect("classify");
        assert_eq!(branch, ResponseBranch::Unexpected);
    }
}
