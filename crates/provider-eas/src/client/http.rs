// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use base64::Engine;

use super::{
    EasClient, EasError,
    redaction::{body_dump_allowed, hex_capped, parse_failure_preview},
    retry::parse_retry_after_delta,
    urlencode,
};
use crate::wbxml::{WbxmlElement, deserialize_to_tree, serialize_tree};

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

impl EasClient {
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
    pub(super) async fn send_command_no_retry(
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
        let auth_header = if let Some(auth) = &self.config.auth {
            auth.authorization_header().await?
        } else {
            let auth_value = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.config.username, self.config.password));
            format!("Basic {auth_value}")
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
            .map_or(0, |d| u64::cast_signed(d.as_secs()));
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

        log::debug!("EAS response: status={status}, content-type={content_type}");

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
                    message: format!("protocol error from server: {s}"),
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
                "server returned non-WBXML content-type '{content_type}'. First 200 bytes: {preview}"
            )));
        }

        let body = response.bytes().await?;
        if body.is_empty() {
            log::debug!("EAS {cmd_name} response: empty body");
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- F4: SendMail family — empty body is success ----

    #[test]
    fn empty_body_allowed_for_compose_commands() {
        assert!(empty_body_outcome(true).is_none());
        assert!(matches!(
            empty_body_outcome(false),
            Some(EasError::Transport(_))
        ));
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
