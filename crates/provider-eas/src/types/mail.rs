// SPDX-License-Identifier: MPL-2.0
//! Compose-command types (SendMail / SmartForward / SmartReply) and the
//! [MS-ASCMD] ClientId synthesis helpers.

use serde::{Deserialize, Serialize};

use super::sync::default_true;
// ---------- SendMail / SmartForward / SmartReply ----------

/// SendMail request ([MS-ASCMD] §2.2.1.19): one raw RFC 5322 message,
/// uploaded as an opaque MIME blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMailRequest {
    /// Raw RFC 5322 message bytes. Emitted on the wire as a WBXML OPAQUE
    /// `<Mime>` element (token 0x10, page 21) — NOT a base64 string. EAS
    /// mandates OPAQUE for `<Mime>`: the server treats STR_I `<Mime>` as
    /// truncated/inline-text, which silently corrupts binary MIME.
    pub mime: Vec<u8>,
    /// If true, emit `<SaveInSentItems/>` so the server stores a Sent copy.
    /// EAS servers save automatically when this is present; the client must
    /// NOT also IMAP-APPEND (see `Capabilities::saves_sent_automatically`).
    #[serde(default = "default_true")]
    pub save_to_sent: bool,
    /// Optional client-generated correlation id (e.g. `"SendMail-{uuid}"`).
    /// Emitted as `<ClientId>` (STR_I) when `Some`. [MS-ASCMD] caps the value
    /// at 40 characters and servers DO enforce it — Exchange 15.2 rejects an
    /// over-cap ClientId with in-body Status 103 (task-11 live evidence: a
    /// 45-char `"SendMail-{uuid}"` send was rejected and the mail silently
    /// never existed). Synthesize via [`new_send_client_id`], which clamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// [MS-ASCMD] ClientId length cap: "The ClientId element value can be up to
/// 40 characters in length." Exchange 15.2 enforces this with in-body Status
/// 103 (task-11 live evidence) — every synthesized ClientId must fit.
pub const CLIENT_ID_MAX_LEN: usize = 40;

/// Synthesize a compose-command ClientId (`SendMail` / `SmartForward`
/// degrade / `SmartReply`) guaranteed to fit the [MS-ASCMD] 40-char cap
/// ([`CLIENT_ID_MAX_LEN`]): `{prefix}{uuid-simple}` with the 32-hex-char
/// simple-uuid truncated as needed. A `prefix` longer than the cap minus 8
/// is itself truncated so at least 8 chars of uuid entropy survive.
pub fn new_send_client_id(prefix: &str) -> String {
    const MIN_ENTROPY: usize = 8;
    let prefix_budget = CLIENT_ID_MAX_LEN - MIN_ENTROPY;
    let prefix = if prefix.len() > prefix_budget {
        &prefix[..prefix_budget]
    } else {
        prefix
    };
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let take = (CLIENT_ID_MAX_LEN - prefix.len()).min(uuid.len());
    format!("{prefix}{}", &uuid[..take])
}

/// Synthesize a Calendar Sync-Add ClientId (`"CalAdd-"` + simple uuid = 39
/// chars, under the [MS-ASCMD] 40-char cap with no clamping needed) — the
/// sibling of [`new_send_client_id`] for the M8 calendar upsync Add command.
/// The added item has no ServerId yet, so the server correlates its
/// response through this id.
pub fn new_calendar_client_id() -> String {
    new_send_client_id("CalAdd-")
}

/// SmartForward request ([MS-ASCMD] §2.2.1.18): forward the message named
/// by `source_server_id`, sending the forwarded MIME built by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartForwardRequest {
    /// The forwarded message's MIME, base64-encoded.
    pub mime_base64: String,
    /// Server ID of the message being forwarded.
    pub source_server_id: String,
    /// Collection ID (folder) containing the source message.
    pub source_collection_id: String,
    /// If true, emit `<SaveInSentItems/>` so the server stores a Sent copy.
    #[serde(default = "default_true")]
    pub save_to_sent: bool,
    /// If true, replace the source MIME rather than appending to it.
    #[serde(default)]
    pub replace_mime: bool,
    /// Client-generated correlation id (e.g. `"SmartForward-{uuid}"`), emitted
    /// as `<ClientId>` when `Some`. Exchange 15.2 rejects compose commands
    /// without a ClientId with in-body Status 103 (F10-3 live evidence) —
    /// callers should always set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// SmartReply request ([MS-ASCMD] §2.2.1.20): reply to the message named
/// by `source_server_id`, sending the reply MIME built by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartReplyRequest {
    /// The reply's MIME, base64-encoded.
    pub mime_base64: String,
    /// Server ID of the message being replied to.
    pub source_server_id: String,
    /// Collection ID (folder) containing the source message.
    pub source_collection_id: String,
    /// If true, emit `<SaveInSentItems/>` so the server stores a Sent copy.
    #[serde(default = "default_true")]
    pub save_to_sent: bool,
    /// If true, replace the source MIME rather than appending to it.
    #[serde(default)]
    pub replace_mime: bool,
    /// See `SmartForwardRequest::client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- task-11 fix-round: ClientId ≤ 40-char cap ([MS-ASCMD]; Exchange
    // 15.2 enforces with in-body Status 103 — the 45-char "SendMail-{uuid}"
    // production id was a live-verified phantom send) ----

    #[test]
    fn new_send_client_id_fits_40_char_cap_for_all_production_prefixes() {
        for prefix in ["SM", "SFWD-", "SendMail-", "SR-"] {
            let id = new_send_client_id(prefix);
            assert!(
                id.len() <= CLIENT_ID_MAX_LEN,
                "ClientId {id:?} is {} chars — over the [MS-ASCMD] 40-char cap",
                id.len()
            );
            assert!(id.starts_with(prefix), "{id:?} lost its prefix {prefix:?}");
        }
    }

    #[test]
    fn new_send_client_id_is_unique_per_call() {
        let a = new_send_client_id("SM");
        let b = new_send_client_id("SM");
        assert_ne!(a, b, "two synthesized ClientIds must not collide");
    }

    #[test]
    fn new_send_client_id_clamps_overlong_prefix_but_keeps_entropy() {
        let prefix = "P".repeat(100);
        let id = new_send_client_id(&prefix);
        assert_eq!(id.len(), CLIENT_ID_MAX_LEN);
        // Prefix truncated to cap-8 so ≥8 chars of uuid entropy survive.
        assert!(id[..CLIENT_ID_MAX_LEN - 8].chars().all(|c| c == 'P'));
    }

    /// M8 calendar upsync Task 2: the Sync-Add ClientId constructor —
    /// sibling of `new_send_client_id` with the fixed "CalAdd-" prefix
    /// (7 + 32-hex uuid = 39, under the cap with no clamping needed).
    #[test]
    fn new_calendar_client_id_fits_cap_carries_prefix_and_is_unique() {
        let a = new_calendar_client_id();
        let b = new_calendar_client_id();
        for id in [&a, &b] {
            assert!(
                id.len() <= CLIENT_ID_MAX_LEN,
                "ClientId {id:?} is {} chars — over the [MS-ASCMD] 40-char cap",
                id.len()
            );
            assert!(id.starts_with("CalAdd-"), "{id:?} lost its prefix");
        }
        assert_ne!(a, b, "two synthesized calendar ClientIds must not collide");
    }
}
