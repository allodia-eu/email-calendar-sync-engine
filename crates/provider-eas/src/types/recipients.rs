// SPDX-License-Identifier: MPL-2.0
//! ResolveRecipients request/response types.

use serde::{Deserialize, Serialize};
// ---------- ResolveRecipients ----------

fn default_resolve_recipients_status() -> u32 {
    1
}

/// Request for the ResolveRecipients command ([MS-ASCMD] §2.2.1.15 / §4.18):
/// resolves a list of ambiguous-name (ANR) strings and/or SMTP addresses to
/// directory entries (GAL + contacts) and can fetch their free/busy data.
/// Scope: recipient resolution + availability. Certificate retrieval is NOT
/// requested (the parser reads a Certificates node's status/count only);
/// pictures are out of scope.
///
/// PRIVACY: `to` entries are directory lookup strings — never dump this
/// struct into a log line; errors carry status codes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResolveRecipientsRequest {
    /// One To element per entry to resolve (§2.2.3.191; the schema §6.31
    /// allows 1..100, each ≤256 chars). ANR prefix strings ("Testers") or
    /// full SMTP addresses. REQUIRED: the client rejects an empty list
    /// before any network I/O — a ResolveRecipients without a To is
    /// pointless.
    pub to: Vec<String>,
    /// Options > MaxAmbiguousRecipients (§2.2.3.103, 0..=9999): caps the
    /// ambiguous-match suggestions returned per To. `None` omits the
    /// element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ambiguous_recipients: Option<u32>,
    /// Options > Availability window (§2.2.3.16): (StartTime, EndTime) as
    /// ISO-8601 UTC strings. `None` omits the whole Availability element
    /// (no free/busy requested). Both fields always emit together — the
    /// schema (§6.31) makes StartTime REQUIRED once Availability is
    /// present. Serialized over IPC as a JSON `[start, end]` pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<(String, String)>,
}

/// One resolved recipient entry (§2.2.3.144 Recipient).
///
/// PRIVACY: `display_name` / `email_address` are directory PII — never log
/// this struct wholesale (its `Debug` impl prints them); errors carry
/// status codes only.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResolvedRecipient {
    /// Recipient > Type (§2.2.3.186.5): 1 = GAL entry, 2 = contact entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_type: Option<u32>,
    /// Recipient > DisplayName (§2.2.3.49.6) — directory PII, never log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Recipient > EmailAddress (§2.2.3.55.2) — directory PII, never log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,
    /// Availability > Status (§2.2.3.177.12): 1 = free/busy retrieved (does
    /// not imply completeness), 160 = over the exact-match availability
    /// limit, 161 = DL over 20 members, 162 = temporary retrieval failure
    /// (the client SHOULD reissue). `None` when the Recipient carries no
    /// Availability element — ambiguous-match suggestions (Response Status
    /// 2/3) never carry one (§4.18.4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_status: Option<u32>,
    /// Availability > MergedFreeBusy (§2.2.3.109): the digit string is
    /// preserved VERBATIM (one digit per time slot: 0 free, 1 tentative,
    /// 2 busy, 3 OOF, 4 no data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_free_busy: Option<String>,
    /// Certificates > Status (§2.2.3.177.12): 1 = certificates returned.
    /// This client never REQUESTS certificates, but if a server sends the
    /// node anyway its status is surfaced here. BY DESIGN the certificate
    /// bytes themselves (Certificate / MiniCertificate) are NOT parsed —
    /// status/count only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificates_status: Option<u32>,
    /// Certificates > CertificateCount (§2.2.3.21): number of valid
    /// certificates the server returned. See `certificates_status` — the
    /// bytes are not captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_count: Option<u32>,
}

/// One per-To Response element (§2.2.3.153.6). The response carries one
/// Response sibling per request To, in request order (§4.18.4.2).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResolveRecipientsResponse {
    /// Response > To (§2.2.3.191): echoes the request's To entry.
    pub to: String,
    /// Response > Status (§2.2.3.177.12): 1 = resolved, 2 = ambiguous
    /// (suggestions returned), 3 = ambiguous partial list (RecipientCount
    /// carries the true total), 4 = no match. Non-1 is DATA, not an
    /// error — the caller prompts the user to pick a suggestion.
    pub status: u32,
    /// Response > RecipientCount (§2.2.3.146): total matches server-side
    /// (can exceed `recipients.len()` for ambiguous partial lists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_count: Option<u32>,
    /// Response > Recipient elements (§2.2.3.144), wire order.
    #[serde(default)]
    pub recipients: Vec<ResolvedRecipient>,
}

/// Result of the ResolveRecipients command ([MS-ASCMD] §4.18).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResolveRecipientsResult {
    /// Command-level Status (§2.2.3.177.12): 1 = success (the default when
    /// the element is absent, mirroring the GetItemEstimate/Settings family
    /// convention — §6.32 makes it required, so the default only guards
    /// lenient servers), 5 = protocol error, 6 = server error (SHOULD
    /// retry). Non-1 is surfaced as `EasError::CommandStatus` by the
    /// client, mirroring the ValidateCert/Settings family.
    #[serde(default = "default_resolve_recipients_status")]
    pub status: u32,
    /// One Response sibling per request To (§4.18.4.2), wire order. Empty
    /// on a command-level rejection.
    #[serde(default)]
    pub responses: Vec<ResolveRecipientsResponse>,
}
