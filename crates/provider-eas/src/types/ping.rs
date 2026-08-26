// SPDX-License-Identifier: MPL-2.0
//! Ping heartbeat request/response types ([MS-ASPING]).

use serde::{Deserialize, Serialize};
// ---------- Ping ----------

/// Ping request ([MS-ASPING] §2.2.1): the long-poll change-notification
/// command — the server holds the connection for the heartbeat interval or
/// until a monitored collection changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingRequest {
    /// Heartbeat interval in seconds (60-3540). Server will hold the connection
    /// for this duration or until a change occurs.
    pub heartbeat_interval: u32,
    /// Collections to monitor for changes.
    pub monitored_collections: Vec<PingCollection>,
}

/// One collection named in a Ping request's `Folders` list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingCollection {
    /// ServerId of the folder to monitor.
    pub collection_id: String,
    /// Item class to monitor within the folder (`"Email"`, `"Calendar"`, …).
    pub class: String,
}

/// Result of the Ping command: the wire status mapped to its canonical
/// string plus, on change, the folders that changed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingResult {
    /// `"Expired"` (wire status 1 — heartbeat elapsed with NO changes),
    /// `"Changes"` (wire status 2 — changes found in one or more folders),
    /// or the raw status text for any other code (MS-ASPing / MS-ASCMD
    /// 2.2.3.177.11; mapped by `parse_ping_response`, which also defaults a
    /// missing Status element to `"Expired"`).
    pub status: String,
    /// Server-provided heartbeat interval, present when status is "5"
    /// (requested interval out of range) per MS-ASPing.
    #[serde(default)]
    pub heartbeat_interval: Option<u32>,
    /// Collection ServerIds the server reports as CHANGED (the `Folders` >
    /// `Folder` text values). Per MS-ASCMD the Folders element only appears
    /// when changes occurred — and some servers (dev.cmmp.hksarg, live
    /// evidence 2026-08-03) send it alongside `<Status>2</Status>` on
    /// multi-collection pings, so a non-empty list is a change signal in its
    /// own right, independent of `status`.
    #[serde(default)]
    pub folders: Vec<String>,
    /// NOT a wire field — stamped by `EasClient::ping` when its status-5
    /// retry adopted a server-mandated heartbeat interval, so the engine's
    /// ping loop can persist the adopted value (previously the status-5
    /// interval was discarded after the retry). Skipped by serde: this type
    /// is also the `eas_ping` IPC payload and the field is engine-internal.
    #[serde(skip)]
    pub adopted_heartbeat: Option<u32>,
}
