// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use base64::Engine;

use super::{EasClient, EasError, expect_root};
use crate::{
    commands,
    types::{SendMailRequest, SmartForwardRequest, SmartReplyRequest},
};

const PAGE_COMPOSE: u8 = 21;
const CM_SEND_MAIL: u8 = 0x05;
const CM_SMART_FORWARD: u8 = 0x06;
const CM_SMART_REPLY: u8 = 0x07;

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

impl EasClient {
    /// SendMail — send a single MIME message. Success per MS-ASCMD is an
    /// HTTP 200 with an EMPTY body (we return status 1); a WBXML body is
    /// only present on failure and carries the Status.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::WbxmlError;

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
}
