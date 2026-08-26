// SPDX-License-Identifier: MPL-2.0
//! Sync collection requests/results and the GetItemEstimate payloads.

use serde::{Deserialize, Serialize};

use crate::{calendar::CalendarEventProps, contacts::ContactsContactProps};
// ---------- Sync ----------

/// One element named in a Sync `Supported` list ([MS-ASCMD] §2.2.3.179).
///
/// `page` / `token` identify the schema element by its WBXML code-page tag;
/// the values come from the tables in `eas::wbxml::code_pages` (e.g.
/// Contacts `JobTitle` = page 1, 0x28 and `OfficeLocation` = page 1, 0x2C —
/// the [MS-ASCMD] §4.24 example list). In the wire request each entry is
/// emitted as an empty tag inside `airsync:Supported` ([MS-ASCMD]
/// §2.2.3.179): elements NOT listed become "ghosted", so a later Change
/// omitting one preserves its server-side value instead of deleting it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupportedElement {
    /// WBXML code page of the element (e.g. 1 = Contacts) — see
    /// `eas::wbxml::code_pages`.
    pub page: u8,
    /// Token of the element within `page` — see `eas::wbxml::code_pages`.
    pub token: u8,
}

/// One collection of a Sync request ([MS-ASSYNC] §2.2.1): the sync key to
/// resume from, the class being synced, and the Options shaping the
/// response bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    /// Collection (folder) ServerId to sync.
    pub collection_id: String,
    /// Server-issued sync key from the previous round ("0" starts fresh).
    pub sync_key: String,
    /// `"Email"`, `"Calendar"`, `"Contacts"`.
    pub class: String,
    /// Window size — number of items to fetch per round-trip.
    #[serde(default = "default_window_size")]
    pub window_size: u32,
    /// Optional filter: number of days back to sync (`0` = no filter).
    #[serde(default)]
    pub filter_age_days: u32,
    /// Whether to fetch bodies (`true`) or just headers (`false`).
    #[serde(default = "default_true")]
    pub fetch_body: bool,
    /// Optional AirSyncBase `TruncationSize` (bytes) emitted inside the
    /// BodyPreference container — caps the body payload the server returns
    /// per item (MS-ASWBXML §2.1.2.1.18, token 0x07 on code page 17). Only
    /// takes effect when `fetch_body` is true. `None` keeps the request
    /// byte-for-byte identical to the pre-truncation shape (Type only).
    /// Android uses 204800 (200KB).
    #[serde(default)]
    pub truncation_size: Option<u32>,
    /// `airsync:MIMESupport` ([MS-ASCMD] §2.2.3.110.3): 0 = never send MIME
    /// data, 1 = MIME for S/MIME messages only (regular body for the rest),
    /// 2 = MIME for all messages. Emitted inside Options AFTER BodyPreference
    /// (§2.2.3.125.6 order) only when `Some`; `None` keeps the request
    /// byte-for-byte identical to the pre-MIME shape (absent means 0 on the
    /// server side).
    #[serde(default)]
    pub mime_support: Option<u8>,
    /// `airsync:MIMETruncation` ([MS-ASCMD] §2.2.3.111): truncation level for
    /// MIME data, 0-8 (0 = truncate all body text, 1 = over 4096 chars, …,
    /// 8 = do not truncate, send complete MIME data). Emitted inside Options
    /// after MIMESupport (§2.2.3.125.6 order) only when `Some`.
    #[serde(default)]
    pub mime_truncation: Option<u8>,
    /// `airsync:Supported` ([MS-ASCMD] §2.2.3.179 / §4.24): the schema
    /// elements the client supports for this collection class, listed by
    /// WBXML (page, token). Elements NOT listed become "ghosted": when a
    /// later Change omits a ghosted element the server PRESERVES its value
    /// instead of deleting it — the data-loss hazard the pre-edit work
    /// removes. Emitted inside `Collection` between CollectionId and
    /// DeletesAsMoves (§2.2.3.29.2 strict order) as empty child tags, only
    /// when `Some(non-empty)`. `None` / `Some([])` keep the request
    /// byte-identical to the pre-Supported shape — per §2.2.3.179 rule 1 an
    /// ABSENT Supported means nothing is ghosted, and an empty list maps to
    /// absence (a wire-level `<Supported/>` would mean "ghost everything"
    /// per rule 3, which is deliberately unreachable through this API).
    /// The sync engine passes `None` today; this field is the wire
    /// foundation for contact/calendar editing.
    #[serde(default)]
    pub supported: Option<Vec<SupportedElement>>,
}

/// Serde default for [`SyncRequest::window_size`]: 100, the [MS-ASCMD]
/// §3.1.5.4 / §2.2.3.199 optimum — the server behaves as if an omitted
/// WindowSize were 100, values below 100 cost extra round-trips and battery,
/// values above risk oversized, error-prone responses. The upstream drain
/// loop (`sync_engine/eas_source.rs` in the kylins-client tree) overrides
/// this with its
/// 10→512 doubling ladder; the default lands on the wire only for the
/// direct `eas_sync` command path and other serde-default constructions.
fn default_window_size() -> u32 {
    100
}

pub(super) fn default_true() -> bool {
    true
}

/// Result of one Sync round-trip: the next sync key plus per-class item
/// deltas — Email items in `added`/`updated`, Calendar and Contacts items in
/// their class-specific vectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    /// Next sync key — persist it and send it back on the following Sync.
    pub sync_key: String,
    /// Email-class items added since the previous key.
    pub added: Vec<EasItem>,
    /// Email-class items updated since the previous key.
    pub updated: Vec<EasItem>,
    /// ServerIds deleted since the previous key (shared by every class —
    /// deletes are class-agnostic on the wire).
    pub deleted_server_ids: Vec<String>,
    /// Calendar-class items (populated only when the request class is
    /// "Calendar"; Email syncs keep these empty). ServerId travels in the
    /// wrapper so the engine can key rows without touching props.
    /// Deletes stay class-agnostic on the wire — they share
    /// `deleted_server_ids` for every class (M8 Task 4 seam).
    /// Calendar items added since the previous key.
    #[serde(default)]
    pub calendar_added: Vec<CalendarItemWithId>,
    /// Calendar items updated since the previous key.
    #[serde(default)]
    pub calendar_updated: Vec<CalendarItemWithId>,
    /// Contacts-class items (populated only when the request class is
    /// "Contacts"; Email/Calendar syncs keep these empty). Mirrors the
    /// Calendar seam: ServerId travels in the wrapper so the engine can key
    /// rows without touching props. Deletes stay class-agnostic on the wire
    /// — they share `deleted_server_ids` for every class (M8-C task 1 seam).
    /// Contacts items added since the previous key.
    #[serde(default)]
    pub contacts_added: Vec<ContactsItemWithId>,
    /// Contacts items updated since the previous key.
    #[serde(default)]
    pub contacts_updated: Vec<ContactsItemWithId>,
    /// True if more items are available — caller should re-issue Sync with the new sync_key.
    pub more_available: bool,
    /// EAS Sync collection status (MS-ASSYNC 2.2.3.23). `1` = success;
    /// anything else is a protocol error the engine must surface. Defaults
    /// to `1` so the unparsed-stub path (which returns `SyncResult::default()`)
    /// reads as success until real status parsing is wired into the Sync-response
    /// parser.
    #[serde(default = "default_sync_status")]
    pub status: u32,
}

fn default_sync_status() -> u32 {
    1
}

/// Manual `Default` so `status` defaults to `1` (success) rather than the
/// `u32` default of `0`. The `#[serde(default = "...")]` attribute only covers
/// deserialization, not `Default::default()`, so without this impl callers
/// writing `SyncResult::default()` would get `status = 0` (which the engine
/// treats as an error).
impl Default for SyncResult {
    fn default() -> Self {
        Self {
            sync_key: String::default(),
            added: Vec::default(),
            updated: Vec::default(),
            deleted_server_ids: Vec::default(),
            calendar_added: Vec::default(),
            calendar_updated: Vec::default(),
            contacts_added: Vec::default(),
            contacts_updated: Vec::default(),
            more_available: bool::default(),
            status: default_sync_status(),
        }
    }
}

/// One Calendar-class downsync item with its wire ServerId attached — the
/// payload of [`SyncResult::calendar_added`] / [`SyncResult::calendar_updated`]
/// (M8 Task 4 seam). The ServerId travels in the wrapper rather than inside
/// [`CalendarEventProps`] so the engine can key store rows (uid = ServerId,
/// per the Task-1 ruling) without touching the typed props.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarItemWithId {
    /// Server-assigned item id — the engine's store-row key.
    pub server_id: String,
    /// The parsed calendar properties.
    pub props: CalendarEventProps,
}

/// One Contacts-class downsync item with its wire ServerId attached — the
/// payload of [`SyncResult::contacts_added`] / [`SyncResult::contacts_updated`]
/// (M8-C task 1 seam). Mirrors [`CalendarItemWithId`]: the ServerId travels
/// in the wrapper rather than inside [`ContactsContactProps`] so the engine
/// can key store rows (uid = ServerId) without touching the typed props.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsItemWithId {
    /// Server-assigned item id — the engine's store-row key.
    pub server_id: String,
    /// The parsed contact properties.
    pub props: ContactsContactProps,
}

/// Typed email item envelope. Replaces the previous `HashMap<String, String>`
/// payload so the WBXML Sync-response parser can dispatch on typed fields
/// rather than stringly-typed tag names. Only the Email class is modeled here;
/// Calendar/Contacts sync stays deferred.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EasItem {
    /// Server-assigned item id (AirSync:ServerId).
    pub server_id: String,
    /// `email:Subject`, when present.
    pub subject: Option<String>,
    /// `email:From`, when present.
    pub from: Option<String>,
    /// `email:To` recipients, when present.
    pub to: Option<String>,
    /// `email:Cc` recipients, when present.
    pub cc: Option<String>,
    /// `email2:Bcc` recipients, when present.
    pub bcc: Option<String>,
    /// `email:Reply-To`, when present.
    pub reply_to: Option<String>,
    /// `email:DateReceived` (xs:dateTime string, verbatim), when present.
    pub date_received: Option<String>,
    /// `email:Read` ("1"/"0" on the wire), when present.
    pub read: Option<bool>,
    /// `email:Flag` follow-up state (`Some(true)` only for an active flag —
    /// see the parser's Flag.Status rule), when a Flag element was present.
    pub flag: Option<bool>,
    /// `email:Importance` (0=low, 1=normal, 2=high), when present.
    pub importance: Option<u8>,
    /// `airsyncbase:Body` Type 2 (HTML) content, when requested and present.
    pub body_html: Option<String>,
    /// `airsyncbase:Body` Type 1 (plain text) content, when requested and
    /// present.
    pub body_text: Option<String>,
    /// Raw MIME body (`AirSyncBase:Body` Type 4, [MS-ASCMD] §2.2.3.110.3):
    /// the full RFC 5322 message as a MIME BLOB, returned when the sync
    /// Options advertise `MIMESupport` + `BodyPreference` Type 4. Its own
    /// slot — a Type-4 body never also fills `body_html`/`body_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_mime: Option<String>,
    /// `airsyncbase:Truncated` ("1"/"0") — the body was cut at
    /// TruncationSize, when present.
    pub body_truncated: Option<bool>,
    /// `airsyncbase:Preview` text, when present.
    pub preview: Option<String>,
    /// True when the item carried at least one `airsyncbase:Attachment`.
    pub has_attachments: bool,
    /// Attachment metadata parsed from the `airsyncbase:Attachments` subtree.
    pub attachments: Vec<EasAttachment>,
    /// `email2:ConversationId` — opaque server bytes, carried verbatim.
    pub conversation_id: Option<Vec<u8>>,
    /// `email2:IsDraft` ("1"/"0"), when present.
    pub is_draft: Option<bool>,
    /// `email:MessageID` (RFC 5322), when present.
    pub message_id: Option<String>,
    /// `email:MessageClass` ([MS-ASEMAIL] §2.2.2.46): the Outlook/Exchange
    /// message class — `IPM.Note` for ordinary mail,
    /// `IPM.Schedule.Meeting.Request` for invitations, … `None` when the
    /// server omitted the element (the engine then defaults to `IPM.Note`,
    /// matching the spec's default for the element).
    #[serde(default)]
    pub message_class: Option<String>,
    /// `email2:MeetingMessageType` ([MS-ASEMAIL] §2.2.2.47): 0=initial,
    /// 1=full update/request, 2=informational update, 3=outdated,
    /// 4=delegated copy, 5=exception cancellation, 6=exception reply.
    /// [MS-ASCMD] §3.1.5.6: only values 1|2 arm the
    /// Accept/Tentative/Decline response UI (MeetingResponse).
    #[serde(default)]
    pub meeting_message_type: Option<u8>,
    /// `email:MeetingRequest` subtree ([MS-ASEMAIL] §2.2.2.48) — the
    /// meeting logistics for invitation items. `Some` whenever the container
    /// element is present (even if sparse), `None` for non-meeting items.
    #[serde(default)]
    pub meeting: Option<MeetingRequestInfo>,
}

/// The children of an `email:MeetingRequest` container that the reading
/// pane needs to render an invitation banner without a refetch
/// ([MS-ASEMAIL] §2.2.2.48). All fields are `Option` — servers omit the
/// optional children, and absent must stay distinguishable from a
/// present-but-false/empty value. `DtStamp`, `Sensitivity`, `BusyStatus`
/// and the recurrence children are intentionally not modeled (no UI
/// consumer yet). camelCase on the wire like its parent `EasItem`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MeetingRequestInfo {
    /// `email:StartTime` (xs:dateTime string, kept verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// `email:EndTime` (xs:dateTime string, kept verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// `email:Location`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// `email:Organizer` (the organizer's SMTP address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<String>,
    /// `email:ResponseRequested` ("1"/"0" on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_requested: Option<bool>,
    /// `email:AllDayEvent` ("1"/"0" on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_day_event: Option<bool>,
    /// `email:InstanceType` ([MS-ASEMAIL] §2.2.2.36): 0=single occurrence,
    /// 1=master recurring, 2=exception instance, 3=exception master.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<u8>,
    /// The meeting's calendar-identity key — the EXACT-KEY correlation to a
    /// calendar item ([MS-ASEMAIL] §3.1.4.7). At protocol 16.0/16.1 the
    /// `calendar:UID` child of MeetingRequest verbatim ("no conversion is
    /// necessary", [MS-ASWBXML] §2.1.2.1.4 note 4); at ≤14.1 the
    /// `email:GlobalObjId` child (base64) converted to the UID string per
    /// §3.1.4.7 steps 1-5 (`meeting_uid::global_obj_id_to_uid`). `None` when
    /// neither element parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

/// One attachment's metadata from a Sync response
/// (`airsyncbase:Attachment`, [MS-ASAIRS]) — the bytes are fetched
/// separately via ItemOperations using `file_reference`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EasAttachment {
    /// `airsyncbase:FileReference` — the handle an ItemOperations fetch
    /// uses to download the bytes.
    pub file_reference: String,
    /// `airsyncbase:DisplayName` (file name).
    pub display_name: String,
    /// `airsyncbase:ContentId` (inline attachments), when present.
    pub content_id: Option<String>,
    /// `airsyncbase:IsInline` ("1"/"0").
    pub is_inline: bool,
    /// EAS `AirSyncBase:EstimatedDataSize`. Typed as `Option<u32>` so the
    /// parser can distinguish "server omitted it" from "zero". Replaces the
    /// previous untyped `u64` field.
    pub estimated_data_size: Option<u32>,
    /// EAS `AirSyncBase:Method`: 1=Normal, 5=EmbeddedMessage, 6=AttachOLE.
    /// Typed as `Option<u8>` for the same reason as `estimated_data_size`.
    pub method: Option<u8>,
    /// MIME content type, e.g. `"image/png"`. Surfaced on ItemOperations fetch.
    pub content_type: Option<String>,
    /// URL for externally-stored attachments. Rarely populated for mail.
    pub content_location: Option<String>,
}

// ---------- GetItemEstimate ----------

/// GetItemEstimate request ([MS-ASCMD] §2.2.1.7): asks how many items a
/// Sync would bring for one collection, without transferring them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetItemEstimateRequest {
    /// Collection (folder) ServerId to estimate.
    pub collection_id: String,
    /// Current sync key for the collection.
    pub sync_key: String,
    /// Item class within the collection (`"Email"`, `"Calendar"`, …).
    pub class: String,
    /// FilterType day window that would scope the sync (0 = no filter).
    pub filter_age_days: u32,
}

fn default_gie_status() -> u32 {
    1
}

/// Result of the GetItemEstimate command: the estimated item count for the
/// requested collection.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GetItemEstimateResult {
    /// Estimated number of items the next Sync would return.
    pub count: u32,
    /// CollectionId echoed back — which collection the count is for.
    pub collection_id: String,
    /// GIE command status (MS-ASCMD). 1 = success; 3 = sync state not primed
    /// (a Sync must run for the collection first). Defaults to 1 so
    /// pre-fix persisted shapes read as success.
    #[serde(default = "default_gie_status")]
    pub status: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serde-default WindowSize must be 100 — the [MS-ASCMD] §3.1.5.4 /
    /// §2.2.3.199 optimum: the server treats an omitted WindowSize as 100,
    /// values below 100 cost extra round-trips, values above risk oversized
    /// responses. The `eas_sync` Tauri command deserializes the frontend's
    /// minimal JSON, so this default lands on the wire.
    #[test]
    fn sync_request_default_window_size_is_100() {
        let json = r#"{"collection_id":"2","sync_key":"0","class":"Email"}"#;
        let req: SyncRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            req.window_size, 100,
            "serde default WindowSize must be 100 ([MS-ASCMD] §2.2.3.199)"
        );
        // The other defaults ride along unchanged.
        assert_eq!(req.filter_age_days, 0);
        assert!(req.fetch_body);
        assert_eq!(req.truncation_size, None);
    }

    // ---- Task 2 (eas-p3-commands): SyncRequest.supported serde contract ----

    /// Legacy `eas_sync` IPC payloads predate the `supported` field; the
    /// serde default must read them as `None` so the builder omits
    /// `<Supported>` and the wire shape stays byte-identical to today.
    #[test]
    fn sync_request_without_supported_deserializes_as_none() {
        let json = r#"{"collection_id":"2","sync_key":"0","class":"Contacts"}"#;
        let req: SyncRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            req.supported, None,
            "absent `supported` must default to None (no Supported element on the wire)"
        );
    }

    /// A payload carrying `supported` round-trips through serde losslessly
    /// (the (page, token) pairs are plain numbers end to end).
    #[test]
    fn sync_request_supported_round_trips_through_serde() {
        let req = SyncRequest {
            collection_id: "2".to_string(),
            sync_key: "0".to_string(),
            class: "Contacts".to_string(),
            window_size: 5,
            filter_age_days: 0,
            fetch_body: false,
            truncation_size: None,
            mime_support: None,
            mime_truncation: None,
            // §4.24 example list: Contacts JobTitle (page 1, 0x28) +
            // OfficeLocation (page 1, 0x2C) — CONTACTS_TOKENS.
            supported: Some(vec![
                SupportedElement {
                    page: 1,
                    token: 0x28,
                },
                SupportedElement {
                    page: 1,
                    token: 0x2C,
                },
            ]),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: SyncRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, req, "supported must survive a serde round-trip");
    }
}
