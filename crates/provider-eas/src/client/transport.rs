// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{
    EasClient, EasError,
    retry::{
        MAX_REDIRECT_HOPS, RedirectHop, RetryDecision, redirect_hop_decision,
        retry_decision_for_http_err,
    },
};
use crate::{commands, wbxml::WbxmlElement};

impl EasClient {
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
    ///
    /// # Errors
    ///
    /// Returns the final `EasError` after the retry layers: transport failures,
    /// non-2xx HTTP (`HttpStatus`), WBXML decode failures, or a non-success in-body
    /// `CommandStatus` (a provision-demanding status is retried once inside).
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
    pub(super) async fn send_command_ex_opts(
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
    ///
    /// # Errors
    ///
    /// Returns the final `EasError` after the retry layers: transport failures,
    /// non-2xx HTTP (`HttpStatus`), WBXML decode failures, or a non-success in-body
    /// `CommandStatus` (a provision-demanding status is retried once inside).
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
    ///
    /// # Errors
    ///
    /// Returns the final `EasError` after the retry layers: transport failures,
    /// non-2xx HTTP (`HttpStatus`), WBXML decode failures, or a non-success in-body
    /// `CommandStatus` (a provision-demanding status is retried once inside).
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
                        .is_some_and(crate::auth::EasAuth::is_oauth);
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

    /// Internal no-retry send for the Provision/Settings internals:
    /// empty body is always an error here. `timeout` is the per-request
    /// override threaded down to `send_command_no_retry` (`None` = client
    /// default; the Provision/Settings callers all pass `None`).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub(super) async fn send_command_no_retry_tree(
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
}
