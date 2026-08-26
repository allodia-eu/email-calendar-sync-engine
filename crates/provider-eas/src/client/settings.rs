// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{EasClient, EasError, expect_root};
use crate::{
    commands,
    types::{
        DevicePasswordResult, OofResult, OofSettings, ResolveRecipientsRequest,
        ResolveRecipientsResult, UserInformationResult, ValidateCertRequest, ValidateCertResult,
    },
};

impl EasClient {
    /// Settings → DeviceInformation (MS-ASCMD §2.2.1.18): identifies this
    /// device's model/OS so the server can evaluate provisioning policy.
    /// Sent on demand when Provision answers 165 (DeviceInformationRequired).
    /// Uses `send_command_no_retry` for the same anti-recursion invariant as
    /// `provision()` (it is called FROM `provision()`).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
}

#[cfg(test)]
mod tests {
    use engine_tls::TlsClientConfig;

    use super::*;
    use crate::types::EasConfig;

    /// ResolveRecipients with an EMPTY To list is rejected client-side
    /// before any network I/O: §2.2.3.191 requires at least one To, and an
    /// empty request is pointless (nothing to resolve). The error names the
    /// command so the caller's log alone diagnoses the misuse.
    #[tokio::test]
    async fn resolve_recipients_rejects_empty_to_list() {
        let mut client = EasClient::new(EasConfig::default(), &TlsClientConfig::bundled())
            .expect("bundled-roots client build");
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
}
