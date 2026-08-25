// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// Hex-dump helper for wire-level request/response logs, capped so a large
/// Sync body can't flood the log file (Ping bodies are ~20-120 bytes).
pub(super) fn hex_capped(bytes: &[u8], cap: usize) -> String {
    use std::fmt::Write as _;
    let n = bytes.len().min(cap);
    let mut s = String::with_capacity(2 * n + 16);
    for b in &bytes[..n] {
        let _ = write!(s, "{b:02x}");
    }
    if bytes.len() > cap {
        let _ = write!(s, "…(+{}B)", bytes.len() - cap);
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
pub(super) fn body_dump_allowed(command: &str) -> bool {
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
pub(super) fn parse_failure_preview(body: &[u8], command: &str) -> String {
    if body_dump_allowed(command) {
        format!("{:02X?}", &body[..body.len().min(64)])
    } else {
        format!("<redacted:{command}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
