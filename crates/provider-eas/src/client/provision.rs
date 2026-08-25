// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{EasClient, EasError};

impl EasClient {
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
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
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
