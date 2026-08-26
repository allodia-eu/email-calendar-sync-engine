// SPDX-License-Identifier: MPL-2.0
//! Settings (UserInformation, DevicePassword, Oof) and ValidateCert types.

use serde::{Deserialize, Serialize};
// ---------- Settings UserInformation ----------

fn default_user_information_status() -> u32 {
    1
}

/// Result of the Settings → UserInformation Get form ([MS-ASCMD] §4.21):
/// the account's SMTP addresses plus the two Settings status levels.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserInformationResult {
    /// Effective command status. Starts at the top-level settings:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); a UserInformation-level Status, when present,
    /// overrides it (more specific wins — the ItemOperations rule).
    #[serde(default = "default_user_information_status")]
    pub status: u32,
    /// UserInformation-level settings:Status, `None` when the element is
    /// absent (e.g. a command-level rejection carries no UserInformation).
    #[serde(default)]
    pub user_information_status: Option<u32>,
    /// The mailbox's SMTP addresses (settings:SMTPAddress values), in wire order.
    #[serde(default)]
    pub email_addresses: Vec<String>,
}

// ---------- Settings DevicePassword ----------

fn default_device_password_status() -> u32 {
    1
}

/// Result of the Settings → DevicePassword Set form ([MS-ASCMD] §4.22):
/// the server stores (or clears) the device's recovery password and answers
/// with status only — no payload beyond the two Settings status levels.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DevicePasswordResult {
    /// Effective command status. Starts at the top-level settings:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); a DevicePassword-level Status, when present,
    /// overrides it (more specific wins — the ItemOperations rule).
    #[serde(default = "default_device_password_status")]
    pub status: u32,
    /// DevicePassword-level settings:Status (nested under DevicePassword/Set
    /// per the [MS-ASCMD] §4.22.2 wire example; §2.2.3.177.15 also allows it
    /// directly under DevicePassword — the parser accepts both), `None` when
    /// the element is absent (e.g. a command-level rejection carries no
    /// DevicePassword).
    #[serde(default)]
    pub device_password_status: Option<u32>,
}

// ---------- Settings Oof ----------

fn default_oof_result_status() -> u32 {
    1
}

/// Which audience an OOF reply message applies to. Maps 1:1 to the three
/// mutually exclusive AppliesTo* marker elements of the Settings code page
/// ([MS-ASCMD] §2.2.3.123):
/// - `Internal` ↔ `AppliesToInternal` (0x0E) — same-organization senders;
/// - `ExternalKnown` ↔ `AppliesToExternalKnown` (0x0F) — outside senders in the user's contacts;
/// - `ExternalUnknown` ↔ `AppliesToExternalUnknown` (0x10) — outside senders not in the user's
///   contacts.
///
/// Serialized as the plain variant name, which the frontend passes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OofAppliesTo {
    /// Same-organization senders (`AppliesToInternal`).
    Internal,
    /// Outside senders already in the user's contacts
    /// (`AppliesToExternalKnown`).
    ExternalKnown,
    /// Outside senders not in the user's contacts
    /// (`AppliesToExternalUnknown`).
    ExternalUnknown,
}

/// One audience-specific OOF message ([MS-ASCMD] §2.2.3.123). `enabled`
/// maps to settings:Enabled ("1"/"0", §2.2.3.59); `reply_message` is the
/// auto-reply body (private user content — never log it); `body_type` is
/// the wire format string ("Text" / "HTML", §2.2.3.17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OofMessage {
    /// Which audience this message replies to (one of the AppliesTo*
    /// markers).
    pub applies_to: OofAppliesTo,
    /// None when the Enabled element is absent or malformed (§2.2.3.59
    /// allows only "1"/"0"; anything else is warn-logged and kept as None
    /// rather than coerced).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The auto-reply body, when set.
    #[serde(default)]
    pub reply_message: Option<String>,
    /// Wire format of the reply body ("Text" / "HTML"), when set.
    #[serde(default)]
    pub body_type: Option<String>,
}

/// Out-of-office settings ([MS-ASCMD] §4.19): the OofState plus the
/// optional scheduled window and up to three audience messages. Carries the
/// Get-response payload AND the Set-request input. `state` maps to
/// settings:OofState (0 = disabled, 1 = global, 2 = time-based; §2.2.3.124
/// requires 2 when times are present); `start_time`/`end_time` are the
/// ISO-8601 strings exactly as they appear on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OofSettings {
    /// `settings:OofState`: 0 = disabled, 1 = global, 2 = time-based.
    #[serde(default)]
    pub state: Option<u32>,
    /// Scheduled-window start (ISO-8601, wire form), when time-based.
    #[serde(default)]
    pub start_time: Option<String>,
    /// Scheduled-window end (ISO-8601, wire form), when time-based.
    #[serde(default)]
    pub end_time: Option<String>,
    /// One entry per audience, wire order. The Set form MUST NOT repeat an
    /// AppliesTo* across messages (§2.2.3.123); the builder emits whatever
    /// it is given — deduplication is the frontend's job.
    #[serde(default)]
    pub messages: Vec<OofMessage>,
}

/// Result of the Settings → Oof Set form ([MS-ASCMD] §4.19.2): the server
/// answers with status only — no payload beyond the two Settings status
/// levels.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OofResult {
    /// Effective command status. Starts at the top-level settings:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); an Oof-level Status, when present, overrides it
    /// (more specific wins — the ItemOperations rule).
    #[serde(default = "default_oof_result_status")]
    pub status: u32,
    /// Oof-level settings:Status (directly under Oof per the [MS-ASCMD]
    /// §4.19.2.2 wire example; §2.2.3.177.15 names Oof as a valid parent of
    /// settings:Status), `None` when the element is absent (e.g. a
    /// command-level rejection carries no Oof).
    #[serde(default)]
    pub oof_status: Option<u32>,
}

// ---------- ValidateCert ----------

fn default_validate_cert_status() -> u32 {
    1
}

/// Request for the ValidateCert command ([MS-ASCMD] §2.2.1.22 / §4.20.1).
///
/// The server validates one or more X.509 certificates (used to verify
/// S/MIME signatures): it checks expiry, revocation, and walks the chain up
/// to a trusted root.
///
/// * `certificate_chain` — the chain certificates, wire order. Maps to the OPTIONAL
///   validatecert:CertificateChain container (§2.2.3.20); an empty vec omits the element entirely.
/// * `certificates` — the certificates to validate, wire order. Maps to the REQUIRED
///   validatecert:Certificates container (§2.2.3.23.2); the builder emits the container
///   unconditionally, so callers must pass at least one certificate (§2.2.3.23.2 requires 1..N
///   Certificate children).
/// * `check_crl` — maps to the OPTIONAL validatecert:CheckCRL element (§2.2.3.26): `true` emits
///   `<CheckCRL>1</CheckCRL>` (the server MUST NOT ignore an unverifiable revocation status);
///   `false` omits the element.
///
/// SECURITY: the strings are opaque base64-encoded DER payloads. They can be
/// large and are security-sensitive material — never log them (this type's
/// `Debug` impl does print them, so do not interpolate a request into any
/// log line; the transport layer's body dumps are redacted for this command,
/// see `client::body_dump_allowed`). Errors carry status codes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ValidateCertRequest {
    /// Chain certificates (base64 DER), wire order. Empty → no
    /// CertificateChain element.
    #[serde(default)]
    pub certificate_chain: Vec<String>,
    /// Certificates to validate (base64 DER), wire order. Required on the
    /// wire (§2.2.3.23.2); must contain at least one entry.
    pub certificates: Vec<String>,
    /// CheckCRL flag (§2.2.3.26). `false` omits the element.
    #[serde(default)]
    pub check_crl: bool,
}

/// Result of the ValidateCert command ([MS-ASCMD] §4.20.2).
///
/// * `status` — the command-level validatecert:Status (§2.2.3.177.18: 1 = success, 17 = failure).
///   Defaults to 1 when the element is absent, mirroring the GetItemEstimate/Settings family
///   convention.
/// * `certificate_statuses` — one entry per response Certificate element, in document order
///   (correlate with the request order). Per-certificate codes per §2.2.3.177.18: 1 success, 3 bad
///   signature / untrusted source, 4 untrusted issuer, 5 malformed chain, 6 not valid for email
///   signing, 7 expired / not yet valid, 8 inconsistent validity periods, 9 misused chain member. A
///   Certificate element without a parsable Status is warn-logged and skipped — it contributes NO
///   entry (never a fabricated success).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidateCertResult {
    /// Command-level status. 1 = success (default when absent); non-1 is
    /// surfaced as `EasError::CommandStatus` by the client.
    #[serde(default = "default_validate_cert_status")]
    pub status: u32,
    /// Per-certificate validation statuses, response order.
    #[serde(default)]
    pub certificate_statuses: Vec<u32>,
}
