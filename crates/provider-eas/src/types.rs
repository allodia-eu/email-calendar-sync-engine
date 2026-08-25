// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Minimal EAS type set for MVP scope (9 commands: FolderSync, Sync, SendMail,
// SmartForward, SmartReply, ItemOperations, GetItemEstimate, Ping, FolderCreate/Delete/Update).
// Full type coverage (Provision, Settings, Search, ResolveRecipients, ValidateCert,
// Find, AutoDiscover, MeetingResponse) is deferred.

use serde::{Deserialize, Serialize};

use crate::{auth::EasAuth, calendar::CalendarEventProps, contacts::ContactsContactProps};

// ---------- Configuration ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EasConfig {
    /// Full URL to the Exchange ActiveSync endpoint, e.g.
    /// `https://mail.kylins.com/Microsoft-Server-ActiveSync`.
    pub url: String,
    /// Username for Basic auth. For domain accounts use `DOMAIN\user` or `user@domain`.
    pub username: String,
    /// EAS `User` query parameter — the mailbox's primary address per MS-ASHTTP.
    /// Empty → the client falls back to `username` (older configs, DOMAIN\user auth).
    #[serde(default)]
    pub user: String,
    /// Plaintext password (transported over TLS; encrypted at rest via crypto::encrypt).
    pub password: String,
    /// Protocol version: `"2.5"`, `"12.0"`, `"12.1"`, `"14.0"`, `"14.1"`, `"16.0"`, `"16.1"`.
    /// Default `"16.1"` for Exchange 2016/2019/Online.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: String,
    /// Device ID — alphanumeric, max 16 chars. Generated once per install, persisted
    /// in keyring alongside the master key. See `client::device_id()`.
    pub device_id: String,
    /// Device type — `"KylinsMail"` by convention. Sent in the X-MS-DeviceType header.
    #[serde(default = "default_device_type")]
    pub device_type: String,
    /// User-agent string. Defaults to `"KylinsMail/1.0"`.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Policy key returned by Provision command (MVP skips Provision, so this stays `"0"`).
    /// If the server demands provisioning, sync will return status 142; we surface that
    /// to the user as a "policy required" error.
    #[serde(default)]
    pub policy_key: String,
    /// Accept invalid TLS certs (self-signed Exchange servers). Default false.
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Auth strategy selector. `"basic"` (default, historical) uses
    /// `username` / `password`. `"oauth"` means the source layer also fills
    /// `auth` with an `EasAuth::OAuth { .. }` built from the account's stored
    /// OAuth fields. Kept as a free-form `String` (not an enum) so the config
    /// round-trips through serde without a migration when new modes land.
    #[serde(default)]
    pub auth_type: String,
    /// Typed auth payload. Built by `EasSource::eas_config()` when
    /// `auth_type == "oauth"`; the transport calls
    /// `auth.authorization_header()` when `Some`, else falls back to Basic
    /// with `username` / `password`. `None` preserves the historical Basic
    /// path (existing tests construct `EasConfig { .. }` without it).
    #[serde(default)]
    pub auth: Option<EasAuth>,
}

fn default_protocol_version() -> String {
    "16.1".to_string()
}

fn default_device_type() -> String {
    "KylinsMail".to_string()
}

fn default_user_agent() -> String {
    "KylinsMail/1.0".to_string()
}

/// Manual `Default` so adding new optional fields (`auth_type`, `auth`, `user`)
/// doesn't force every construction site to name them. NOTE: the
/// `eas_source::eas_config` literal names every field explicitly (it does NOT
/// use `..Default::default()`), so new fields must be added there too —
/// otherwise the crate fails to compile. The `#[serde(default = "...")]`
/// attributes only cover deserialization, so without this impl,
/// `EasConfig { ..Default::default() }` wouldn't compile.
impl Default for EasConfig {
    fn default() -> Self {
        Self {
            url: String::default(),
            username: String::default(),
            user: String::default(),
            password: String::default(),
            protocol_version: default_protocol_version(),
            device_id: String::default(),
            device_type: default_device_type(),
            user_agent: default_user_agent(),
            policy_key: String::default(),
            accept_invalid_certs: false,
            auth_type: String::default(),
            auth: None,
        }
    }
}

impl EasConfig {
    /// The EAS `User` query param: `user` when set, else `username`.
    pub fn user_param(&self) -> &str {
        if self.user.is_empty() {
            &self.username
        } else {
            &self.user
        }
    }
}

// ---------- Options (server capabilities) ----------

/// Result of an HTTP OPTIONS round-trip against the EAS endpoint
/// ([MS-ASHTTP] §2.2.1.1): the server's advertised protocol versions and
/// supported command list. Used at account setup to negotiate the protocol
/// version (`client::pick_protocol_version`) before any WBXML command runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EasServerOptions {
    /// Entries of the `MS-ASProtocolVersions` header, comma-split and trimmed
    /// (e.g. `["2.5","12.0","12.1","14.0","14.1","16.0","16.1"]`). Empty when
    /// the header was absent.
    pub protocol_versions: Vec<String>,
    /// Entries of the `MS-ASProtocolCommands` header, comma-split and trimmed
    /// (e.g. `["Sync","SendMail","Provision", ...]`). Empty when the header
    /// was absent.
    pub commands: Vec<String>,
}

// ---------- Folders (FolderSync) ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EasFolder {
    pub server_id: String,
    pub parent_id: String,
    pub display_name: String,
    /// `"Email"`, `"Calendar"`, `"Contacts"`, `"Tasks"`, `"Notes"`, etc.
    pub class: String,
    /// Raw EAS folder Type byte (MS-ASFD `FolderHierarchy:Type`): 2=Inbox,
    /// 3=Drafts, 4=DeletedItems, 5=Sent, 6=Outbox, 7=Tasks, 8=Calendar,
    /// 9=Contacts, 10/11=Notes/Journal, 1/12=user-created mail, etc. Surfaced so
    /// the frontend can derive a canonical role without locale-dependent
    /// name-matching. `None` when the element is absent or non-numeric.
    #[serde(default)]
    pub folder_type: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FolderSyncResult {
    /// Command status per [MS-ASCMD] common + FolderSync codes; 1 = success.
    /// Non-success is surfaced by the client as `EasError::CommandStatus`.
    #[serde(default)]
    pub status: u32,
    /// Updated sync key to persist for the next FolderSync call.
    pub sync_key: String,
    /// Folders added or updated since the last sync key.
    pub changes: Vec<EasFolder>,
    /// Server IDs of folders deleted since the last sync key.
    pub deletions: Vec<String>,
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub collection_id: String,
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
/// values above risk oversized, error-prone responses. The engine's own
/// drain loop (sync_engine/eas_source.rs) overrides this with its
/// 10→512 doubling ladder; the default lands on the wire only for the
/// direct `eas_sync` command path and other serde-default constructions.
fn default_window_size() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub sync_key: String,
    pub added: Vec<EasItem>,
    pub updated: Vec<EasItem>,
    pub deleted_server_ids: Vec<String>,
    /// Calendar-class items (populated only when the request class is
    /// "Calendar"; Email syncs keep these empty). ServerId travels in the
    /// wrapper so the engine can key rows without touching props.
    /// Deletes stay class-agnostic on the wire — they share
    /// `deleted_server_ids` for every class (M8 Task 4 seam).
    #[serde(default)]
    pub calendar_added: Vec<CalendarItemWithId>,
    #[serde(default)]
    pub calendar_updated: Vec<CalendarItemWithId>,
    /// Contacts-class items (populated only when the request class is
    /// "Contacts"; Email/Calendar syncs keep these empty). Mirrors the
    /// Calendar seam: ServerId travels in the wrapper so the engine can key
    /// rows without touching props. Deletes stay class-agnostic on the wire
    /// — they share `deleted_server_ids` for every class (M8-C task 1 seam).
    #[serde(default)]
    pub contacts_added: Vec<ContactsItemWithId>,
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
    pub server_id: String,
    pub props: CalendarEventProps,
}

/// One Contacts-class downsync item with its wire ServerId attached — the
/// payload of [`SyncResult::contacts_added`] / [`SyncResult::contacts_updated`]
/// (M8-C task 1 seam). Mirrors [`CalendarItemWithId`]: the ServerId travels
/// in the wrapper rather than inside [`ContactsContactProps`] so the engine
/// can key store rows (uid = ServerId) without touching the typed props.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsItemWithId {
    pub server_id: String,
    pub props: ContactsContactProps,
}

/// Typed email item envelope. Replaces the previous `HashMap<String, String>`
/// payload so the WBXML Sync-response parser can dispatch on typed fields
/// rather than stringly-typed tag names. Only the Email class is modeled here;
/// Calendar/Contacts sync stays deferred.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EasItem {
    pub server_id: String,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub bcc: Option<String>,
    pub reply_to: Option<String>,
    pub date_received: Option<String>,
    pub read: Option<bool>,
    pub flag: Option<bool>,
    pub importance: Option<u8>,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    /// Raw MIME body (`AirSyncBase:Body` Type 4, [MS-ASCMD] §2.2.3.110.3):
    /// the full RFC 5322 message as a MIME BLOB, returned when the sync
    /// Options advertise `MIMESupport` + `BodyPreference` Type 4. Its own
    /// slot — a Type-4 body never also fills `body_html`/`body_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_mime: Option<String>,
    pub body_truncated: Option<bool>,
    pub preview: Option<String>,
    pub has_attachments: bool,
    pub attachments: Vec<EasAttachment>,
    pub conversation_id: Option<Vec<u8>>,
    pub is_draft: Option<bool>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EasAttachment {
    pub file_reference: String,
    pub display_name: String,
    pub content_id: Option<String>,
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

// ---------- ItemOperations ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemOperationsFetchRequest {
    /// Server ID of the item to fetch.
    pub server_id: String,
    /// Collection (folder) ID containing the item.
    pub collection_id: String,
    /// For attachment fetches: the FileReference returned in a prior Sync.
    pub file_reference: Option<String>,
    /// For search-result fetches: the `search:LongId` returned in a prior
    /// Search response ([MS-ASCMD] §2.2.3.98.1 / example §4.10.3.3). When
    /// set (and `file_reference` is not), the request is the LongId form:
    /// Store=Mailbox + search:LongId + Options>BodyPreference, with no
    /// CollectionId/ServerId. Precedence in the builder:
    /// `file_reference` > `long_id` > collection/server-id.
    /// `#[serde(default)]` keeps pre-Task-5 IPC payloads (no `longId` key)
    /// deserializing unchanged.
    #[serde(default)]
    pub long_id: Option<String>,
    /// Fetch the item as raw MIME instead of an HTML body (task 3). When
    /// true, the item-fetch branch emits `airsync:MIMESupport` level 2
    /// ("send MIME data for all messages", [MS-ASCMD] §2.2.3.110.3) BEFORE
    /// `airsyncbase:BodyPreference` and switches the BodyPreference Type to
    /// 4 — the §4.10.2.1 request shape. Level 2 (not the spec example's 1):
    /// a MIME fetch wants the raw message for ALL items, not S/MIME-only.
    /// Ignored on the attachment (`file_reference`) and search (`long_id`)
    /// branches — MIME doesn't apply to attachment fetches, and the LongId
    /// shape is kept minimal until a consumer needs it. `#[serde(default)]`
    /// keeps pre-Task-3 IPC payloads (no `mime` key) deserializing unchanged.
    #[serde(default)]
    pub mime: bool,
    /// Opt in to a multipart response ([MS-ASCMD] §2.2.1.10.1): when true,
    /// the ItemOperations POST carries the `MS-ASAcceptMultiPart: T` header
    /// ([MS-ASHTTP] §2.2.1.1.2.5) and the client accepts a
    /// `application/vnd.ms-sync.multipart` body — large payloads arrive as
    /// binary parts referenced by `itemoperations:Part` instead of inline
    /// base64, so a big attachment/body never has to round-trip through the
    /// WBXML string table. The server MAY still answer plain WBXML; both
    /// shapes are handled. `#[serde(default)]` keeps pre-multipart IPC
    /// payloads (no `accept_multipart` key) on the inline path unchanged.
    #[serde(default)]
    pub accept_multipart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ItemOperationsFetchResult {
    pub status: u8,
    /// Raw base64-encoded bytes for attachment fetches, or item fields for item fetches.
    pub data: Option<String>,
    pub content_type: Option<String>,
}

// ---------- ItemOperations EmptyFolderContents ----------

/// ItemOperations → EmptyFolderContents request ([MS-ASCMD] §4.14.4.1):
/// deletes EVERY item in the named folder server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmptyFolderContentsRequest {
    /// Collection (folder) ID whose contents are deleted
    /// (`airsync:CollectionId` inside `EmptyFolderContents`).
    pub collection_id: String,
    /// When true the request carries `Options>DeleteSubFolders` and the
    /// folder's subfolders are deleted too. When false the Options element
    /// is omitted entirely (the server default keeps subfolders).
    #[serde(default)]
    pub delete_sub_folders: bool,
}

fn default_empty_folder_contents_status() -> u32 {
    1
}

/// Result of ItemOperations → EmptyFolderContents ([MS-ASCMD] §4.14.4.2):
/// status pair plus the CollectionId echo confirming which folder was
/// emptied.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmptyFolderContentsResult {
    /// Effective command status. Starts at the top-level itemoperations:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); an EmptyFolderContents-level Status, when present,
    /// overrides it (more specific wins — the ItemOperations rule).
    #[serde(default = "default_empty_folder_contents_status")]
    pub status: u32,
    /// EmptyFolderContents-level itemoperations:Status, `None` when the
    /// element is absent (e.g. a command-level rejection carries no
    /// Response at all).
    #[serde(default)]
    pub empty_status: Option<u32>,
    /// The `airsync:CollectionId` echoed back by the server — confirmation
    /// of the folder whose contents were deleted.
    #[serde(default)]
    pub collection_id: Option<String>,
}

// ---------- ItemOperations Move (conversation move) ----------

/// ItemOperations → Move request ([MS-ASCMD] §4.25.1): moves a whole
/// conversation to another folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMoveRequest {
    /// Destination folder's server id (`itemoperations:DstFldId`).
    pub dst_folder_id: String,
    /// The conversation to move (`itemoperations:ConversationId`) — OPAQUE
    /// server bytes carried verbatim (never base64-decoded or re-encoded),
    /// the same convention as the sync path's `conversation_id`. Over IPC
    /// this serializes as a byte array.
    pub conversation_id: Vec<u8>,
    /// When true the request carries `Options>MoveAlways` and ALL FUTURE
    /// messages of the conversation are moved to the destination folder
    /// too — not just the ones currently on the server. Callers MUST
    /// surface this consequence to the user (it behaves like a persistent
    /// server-side rule). When false the Options element is omitted
    /// entirely.
    #[serde(default)]
    pub move_always: bool,
}

fn default_conversation_move_status() -> u32 {
    1
}

/// Result of ItemOperations → Move ([MS-ASCMD] §4.25.2): status pair plus
/// the ConversationId echo confirming which conversation was moved.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationMoveResult {
    /// Effective command status. Starts at the top-level itemoperations:Status
    /// (1 = success, the default when the element is absent — mirroring
    /// GetItemEstimate); a Move-level Status, when present, overrides it
    /// (more specific wins — the ItemOperations rule).
    #[serde(default = "default_conversation_move_status")]
    pub status: u32,
    /// Move-level itemoperations:Status, `None` when the element is absent
    /// (e.g. a command-level rejection carries no Response at all).
    #[serde(default)]
    pub move_status: Option<u32>,
    /// The `itemoperations:ConversationId` echoed back by the server —
    /// opaque bytes verbatim (accepted in both the opaque-binary and
    /// base64-text wire forms, never decoded), confirming the conversation
    /// that was moved. `None` when absent or empty.
    #[serde(default)]
    pub conversation_id: Option<Vec<u8>>,
}

// ---------- GetItemEstimate ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetItemEstimateRequest {
    pub collection_id: String,
    pub sync_key: String,
    pub class: String,
    pub filter_age_days: u32,
}

fn default_gie_status() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GetItemEstimateResult {
    pub count: u32,
    pub collection_id: String,
    /// GIE command status (MS-ASCMD). 1 = success; 3 = sync state not primed
    /// (a Sync must run for the collection first). Defaults to 1 so
    /// pre-fix persisted shapes read as success.
    #[serde(default = "default_gie_status")]
    pub status: u32,
}

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
    Internal,
    ExternalKnown,
    ExternalUnknown,
}

/// One audience-specific OOF message ([MS-ASCMD] §2.2.3.123). `enabled`
/// maps to settings:Enabled ("1"/"0", §2.2.3.59); `reply_message` is the
/// auto-reply body (private user content — never log it); `body_type` is
/// the wire format string ("Text" / "HTML", §2.2.3.17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OofMessage {
    pub applies_to: OofAppliesTo,
    /// None when the Enabled element is absent or malformed (§2.2.3.59
    /// allows only "1"/"0"; anything else is warn-logged and kept as None
    /// rather than coerced).
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub reply_message: Option<String>,
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
    #[serde(default)]
    pub state: Option<u32>,
    #[serde(default)]
    pub start_time: Option<String>,
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

// ---------- Ping ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingRequest {
    /// Heartbeat interval in seconds (60-3540). Server will hold the connection
    /// for this duration or until a change occurs.
    pub heartbeat_interval: u32,
    /// Collections to monitor for changes.
    pub monitored_collections: Vec<PingCollection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingCollection {
    pub collection_id: String,
    pub class: String,
}

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

// ---------- Search ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    /// search:Name — "Mailbox" or "GAL".
    pub store: String,
    /// FreeText keyword(s) for Mailbox; plain ANR prefix string for GAL.
    pub query: String,
    /// Mailbox only: restrict to one folder (airsync:CollectionId). None = all folders.
    #[serde(default)]
    pub collection_id: Option<String>,
    /// Zero-based "m-n" result window (search:Range), e.g. "0-49".
    pub range: String,
    /// Recurse subfolders (search:DeepTraversal).
    #[serde(default)]
    pub deep_traversal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GalEntry {
    pub display_name: Option<String>,
    pub phone: Option<String>,
    pub office: Option<String>,
    pub title: Option<String>,
    pub company: Option<String>,
    pub alias: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub home_phone: Option<String>,
    pub mobile_phone: Option<String>,
    pub email_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResultItem {
    pub class: Option<String>,
    pub long_id: Option<String>,
    pub collection_id: Option<String>,
    #[serde(default)]
    pub item: Option<EasItem>,
    #[serde(default)]
    pub gal: Option<GalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResult {
    pub status: u32,
    pub store_status: Option<u32>,
    pub range: Option<String>,
    pub total: Option<u32>,
    pub results: Vec<SearchResultItem>,
}

// ---------- SendMail / SmartForward / SmartReply ----------

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartForwardRequest {
    pub mime_base64: String,
    /// Server ID of the message being forwarded.
    pub source_server_id: String,
    /// Collection ID (folder) containing the source message.
    pub source_collection_id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartReplyRequest {
    pub mime_base64: String,
    pub source_server_id: String,
    pub source_collection_id: String,
    #[serde(default = "default_true")]
    pub save_to_sent: bool,
    #[serde(default)]
    pub replace_mime: bool,
    /// See `SmartForwardRequest::client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

// ---------- Folder create/update/delete ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderCreateRequest {
    pub parent_id: String,
    pub display_name: String,
    pub class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderUpdateRequest {
    pub server_id: String,
    pub parent_id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderDeleteRequest {
    pub server_id: String,
}

// NOTE: The legacy `pub struct EasError { status, message, command }` that
// previously lived here was dead code — every live EAS error in the codebase
// flows through `crate::client::EasError` (the `thiserror` enum declared
// in `client.rs`). It was removed in Phase 3b Task 1. If you need to surface
// an EAS error, use `client::EasError`.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_param_prefers_user_then_falls_back_to_username() {
        let mut cfg = EasConfig {
            username: "DOMAIN\\felix".into(),
            user: "felixzhou@example.org".into(),
            ..Default::default()
        };
        assert_eq!(cfg.user_param(), "felixzhou@example.org");
        cfg.user.clear();
        assert_eq!(cfg.user_param(), "DOMAIN\\felix");
    }

    #[test]
    fn config_without_user_field_deserializes_with_empty_user() {
        let json = r#"{"url":"https://x/Microsoft-Server-ActiveSync","username":"u","password":"p","device_id":"d"}"#;
        let cfg: EasConfig = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cfg.user, "");
        assert_eq!(cfg.user_param(), "u");
    }

    // ---- Task 2 (eas-p2-polish): SyncRequest serde defaults ----

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

    // ---- Task 10 (eas-p3-commands): ItemOperationsFetchRequest.accept_multipart ----

    /// Pre-multipart IPC payloads carry no `accept_multipart` key; the serde
    /// default must read them as `false` so those requests go out WITHOUT the
    /// `MS-ASAcceptMultiPart: T` header and keep getting inline (base64 Data)
    /// responses — wire behavior unchanged for existing callers.
    #[test]
    fn item_operations_fetch_without_accept_multipart_defaults_false() {
        let json = r#"{"server_id":"5:1","collection_id":"5","file_reference":null}"#;
        let req: ItemOperationsFetchRequest = serde_json::from_str(json).expect("deserialize");
        assert!(
            !req.accept_multipart,
            "absent `accept_multipart` must default to false (no MS-ASAcceptMultiPart header)"
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
