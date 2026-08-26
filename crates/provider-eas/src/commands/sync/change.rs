// SPDX-License-Identifier: MPL-2.0
// Sync Change (client-to-server upsync) model.

use serde::{Deserialize, Serialize};

use crate::{calendar_write::CalendarEventWrite, commands::EasItem};
// ============================================================================
// Sync Change (client-to-server upsync)
// ============================================================================

/// One client-side item mutation carried by a Sync `Commands > Change`
/// element. `server_id` is the wire identifier (the message's `remote_id`
/// verbatim since M6.5 — the pre-M6.5 hashed-uid / `eas_uid_map` bridge is
/// retired). `read` maps to `email:Read` (0/1);
/// `starred` maps to `email:Flag` — `Some(true)` emits the full task-like
/// Flag container (Status "2", FlagType "FollowUp", tasks-page start/due
/// dates), `Some(false)` an empty `<Flag/>`, `None` no Flag element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EasChange {
    /// Wire ServerId of the item to change.
    pub server_id: String,
    /// New `email:Read` state, when changing it.
    pub read: Option<bool>,
    /// New `email:Flag` state, when changing it.
    pub starred: Option<bool>,
}

/// One client-side Calendar item mutation carried by a Sync Commands
/// request (the upsync direction of [MS-ASSYNC] §2.2.2). OUR vocabulary
/// maps onto the wire commands that act on an existing item:
///
/// - `Add` → wire `airsync:Add` { ClientId, ApplicationData } — the item has no ServerId yet; the
///   server correlates the response through the ClientId.
/// - `Replace` → wire `airsync:Change` carrying ServerId ([MS-ASSYNC] §2.2.2 — the Change command
///   updates an existing item). "Replace" is OUR client-side vocabulary only; there is no wire
///   Replace command.
/// - `Remove` → wire `airsync:Delete` { ServerId } — the server's soft-delete semantics; acceptable
///   for v1 per the M8 design (D1).
#[derive(Debug, Clone, PartialEq)]
pub enum CalendarChange {
    /// Create a new event in the collection.
    Add {
        /// Client-generated correlation id (≤ 40 chars, [MS-ASCMD];
        /// Exchange 15.2 rejects over-cap ids with in-body Status 103 —
        /// task-11 live evidence). Synthesize with
        /// [`new_calendar_client_id`](crate::types::new_calendar_client_id),
        /// which guarantees the cap.
        client_id: String,
        /// The event payload, serialized via
        /// [`build_calendar_application_data`](crate::calendar_write::build_calendar_application_data)
        /// (M8 Task 1).
        props: CalendarEventWrite,
    },
    /// Update an existing event (wire: `airsync:Change` with ServerId).
    Replace {
        /// Wire identifier of the existing item.
        server_id: String,
        /// The event payload, serialized via
        /// [`build_calendar_application_data`](crate::calendar_write::build_calendar_application_data)
        /// (M8 Task 1).
        props: CalendarEventWrite,
    },
    /// Delete an existing event (wire: `airsync:Delete` with ServerId).
    Remove {
        /// Wire identifier of the item to delete.
        server_id: String,
    },
}

/// Outcome of a Sync command that carried client-side `Commands` (the upsync
/// direction). Beyond the rotated `new_key` and the collection `status`, the
/// response Collection MAY itself carry server-side `Commands` ([MS-ASSYNC]
/// §2.2.2 — the server piggybacks pending changes onto the upsync response).
/// Those are surfaced here via the same `parse_item` path the downsync uses;
/// discarding them while adopting the rotated key would silently diverge from
/// the server. Empty when the response carries no Commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncChangeOutcome {
    /// The rotated sync key the server issued for the next round.
    pub new_key: String,
    /// Collection status (MS-ASSYNC §2.2.3.23); 1 = success.
    pub status: u32,
    /// Server-side `Commands` piggybacked on the response: items added.
    pub piggybacked_added: Vec<EasItem>,
    /// Server-side `Commands` piggybacked on the response: items updated.
    pub piggybacked_updated: Vec<EasItem>,
    /// Server-side `Commands` piggybacked on the response: ServerIds deleted.
    pub piggybacked_deleted: Vec<String>,
    /// Per-item Add acknowledgements from the response Collection's
    /// `Responses` element ([MS-ASCMD] §2.2.3.154): the server echoes each
    /// client Add as `Add { ClientId, ServerId?, Status }` (§2.2.3.7.2),
    /// mapping the request's ClientId to the ServerId it assigned. Per
    /// §2.2.3.154 acks are only sent for SUCCESSFUL additions — an Add with
    /// no ack here means success with no id to correlate. Empty when the
    /// response carries no Responses element (the email-upsync shape).
    pub add_acks: Vec<CalendarAddAck>,
    /// Per-item statuses for client Change/Delete commands, from the same
    /// `Responses` element (§2.2.3.24 Change / §2.2.3.42.2 Delete). Per
    /// §2.2.3.154 the server only sends these for FAILED changes and
    /// deletions — absence means success. Empty when the response carries no
    /// Responses element.
    pub item_statuses: Vec<CalendarItemStatus>,
}

impl SyncChangeOutcome {
    /// True when the response carried no server-side Commands (the common case).
    pub fn has_piggybacked(&self) -> bool {
        !(self.piggybacked_added.is_empty()
            && self.piggybacked_updated.is_empty()
            && self.piggybacked_deleted.is_empty())
    }
}

/// Per-item acknowledgement of one client Add, echoed by the server under
/// the response Collection's `Responses` element ([MS-ASCMD] §2.2.3.7.2:
/// "The server then responds with an Add element in a Responses element,
/// which specifies the client ID and the server ID that was assigned to the
/// new item" — the §4.5.3.2 example shape `{ ClientId, ServerId, Status }`).
/// Named for its first consumer, the M8 calendar upsync engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarAddAck {
    /// The request's correlation id (ClientId), echoed verbatim — the key
    /// the caller uses to find its pending add.
    pub client_id: String,
    /// Per-item Status ([MS-ASCMD] §2.2.3.177.17); 1 = success. The raw
    /// value is preserved for the engine's failure-class machinery — deep
    /// retry/discard classification lives in the engine, NOT here.
    /// Item-scoped codes verifiable in docs/Exchange/mscmd.txt §2.2.3.177.17:
    /// 6 = "Error in client/server conversion" (malformed/invalid item —
    /// NOT transient, "stop sending the item"), 8 = "Object not found"
    /// (the CollectionId/ServerId is no longer valid).
    pub status: u32,
    /// The ServerId the server assigned to the new item. `None` when the
    /// Add failed (status != 1) or the element is absent — the server only
    /// assigns an id on success.
    pub server_id: Option<String>,
}

impl CalendarAddAck {
    /// True when the per-item Status is 1 (success) per [MS-ASCMD]
    /// §2.2.3.177.17. Note the surrounding contract of §2.2.3.154: "the
    /// client only receives responses for successful additions … and failed
    /// changes and deletions. When the client does not receive a response,
    /// the client MUST assume that the operation succeeded" — an Add with
    /// NO ack at all also means success; there is simply no id to persist.
    pub fn success(&self) -> bool {
        self.status == 1
    }
}

/// Which client command a `Responses` item answers ([MS-ASCMD] §2.2.3.154:
/// each response "is wrapped in an element with the same name as the
/// operation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseItemKind {
    /// Wire `airsync:Change` ([MS-ASCMD] §2.2.3.24) — answers OUR
    /// [`CalendarChange::Replace`].
    Change,
    /// Wire `airsync:Delete` ([MS-ASCMD] §2.2.3.42.2) — answers OUR
    /// [`CalendarChange::Remove`].
    Delete,
}

/// Per-item status of one client Change or Delete command, echoed under the
/// response Collection's `Responses` element ([MS-ASCMD] §2.2.3.24 Change
/// (Sync) / §2.2.3.42.2 Delete (Sync): `{ ServerId, Status }`). Delete
/// responses are rare on the wire — per §2.2.3.154 the server acks
/// deletions only when they FAIL — so the parser surfaces exactly what
/// arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarItemStatus {
    /// The wire identifier of the item the status answers.
    pub server_id: String,
    /// Per-item Status ([MS-ASCMD] §2.2.3.177.17); 1 = success. Raw value
    /// preserved for the engine's failure-class machinery — deep
    /// retry/discard classification lives in the engine, NOT here (see
    /// [`CalendarAddAck::status`] for the citable item-scoped codes).
    pub status: u32,
    /// Whether this status answers a Change or a Delete.
    pub kind: ResponseItemKind,
}

impl CalendarItemStatus {
    /// True when the per-item Status is 1 (success) per [MS-ASCMD]
    /// §2.2.3.177.17. Per §2.2.3.154 these items are only sent for FAILED
    /// changes and deletions, so `false` here is the actionable case; a
    /// command with NO item status at all also means success.
    pub fn success(&self) -> bool {
        self.status == 1
    }
}

// ---------- Email (page 2) Flag tag ids ([MS-ASWBXML] §2.1.2.1.3) ----------
// `Flag` itself lives in `tags::email::FLAG` (0x3A); its children are not in
// tags.rs, so they are local constants here.
pub(super) const EMAIL_FLAG_STATUS: u8 = 0x3B; // "Status" child of Flag — "2" = flagged
pub(super) const EMAIL_FLAG_TYPE: u8 = 0x3D; // "FlagType" — "FollowUp" is the standard value

// ---------- Tasks (page 9) tag ids used inside email:Flag ----------
// Per [MS-ASWBXML] §2.1.2.1.10 and Android EasSync.java:295-315, an active
// email Flag must carry Start/UtcStart/Due/UtcDue dates from the Tasks page —
// the container switches code page email(2) → tasks(9) mid-stream.
pub(super) const PAGE_TASKS: u8 = 9;
pub(super) const TASK_DUE_DATE: u8 = 0x0C;
pub(super) const TASK_UTC_DUE_DATE: u8 = 0x0D;
pub(super) const TASK_START_DATE: u8 = 0x1E;
pub(super) const TASK_UTC_START_DATE: u8 = 0x1F;

/// Active flags get a due date one week out (Android `DateUtils.WEEK_IN_MILLIS`).
pub(super) const FLAG_DUE_OFFSET_SECS: u64 = 7 * 24 * 60 * 60;
