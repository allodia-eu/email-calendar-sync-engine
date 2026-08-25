// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use base64::Engine;

use super::{EasClient, EasError};
use crate::types::EasServerOptions;

impl EasClient {
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
    ///
    /// # Errors
    ///
    /// Returns `EasError::Transport`/`HttpStatus` when the HTTP round-trip fails
    /// (no WBXML body is involved).
    pub async fn options(&self) -> Result<EasServerOptions, EasError> {
        // Same auth-header selection as send_command_no_retry: typed EasAuth
        // when set, else inline Basic from username/password.
        let auth_header = if let Some(auth) = &self.config.auth {
            auth.authorization_header().await?
        } else {
            let auth_value = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.config.username, self.config.password));
            format!("Basic {auth_value}")
        };

        let url = self.config.url.trim_end_matches('/').to_string();
        log::debug!("EAS OPTIONS {url}");

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
        .map(std::string::ToString::to_string)
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
            .map(std::string::ToString::to_string)
            .collect()
    };
    Ok(EasServerOptions {
        protocol_versions: split(versions),
        commands: split(commands),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
