// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Tag constants and helper functions. Each constant packs a `(page, token)`
// pair into a `u16` — page in the high 8 bits, token in the low 8 bits —
// because that is what the ArkTS `Tags` class produces via `page << 6 | tag`.
// Callers can use these constants or pass `(page, token)` tuples directly to
// the serializer; either form is accepted via the `Into<Tag>` impl.
//
// Only the most commonly-used tags are enumerated here. The full table lives
// in `code_pages.rs`; for ad-hoc tags, construct `WbxmlElement::empty(page, token)`
// directly.

/// Code page indices (0..=25). Source: `Tags` constants in `tags.ts`.
pub mod pages {
    pub const AIRSYNC: u8 = 0x00;
    pub const CONTACTS: u8 = 0x01;
    pub const EMAIL: u8 = 0x02;
    pub const CALENDAR: u8 = 0x04;
    pub const MOVE: u8 = 0x05;
    pub const GIE: u8 = 0x06;
    pub const FOLDER: u8 = 0x07;
    pub const MREQ: u8 = 0x08;
    pub const TASK: u8 = 0x09;
    pub const RECIPIENTS: u8 = 0x0A;
    pub const VALIDATE: u8 = 0x0B;
    pub const CONTACTS2: u8 = 0x0C;
    pub const PING: u8 = 0x0D;
    pub const PROVISION: u8 = 0x0E;
    pub const SEARCH: u8 = 0x0F;
    pub const GAL: u8 = 0x10;
    pub const BASE: u8 = 0x11;
    pub const SETTINGS: u8 = 0x12;
    pub const DOCS: u8 = 0x13;
    pub const ITEMS: u8 = 0x14;
    pub const COMPOSE: u8 = 0x15;
    pub const EMAIL2: u8 = 0x16;
    pub const NOTES: u8 = 0x17;
    pub const RIGHTS: u8 = 0x18;
    pub const FIND: u8 = 0x19;
}

/// A few of the most-used AirSync (page 0) tag ids. Other pages are available
/// via the `pages` module and the `code_pages::code_page()` lookup.
pub mod airsync {
    pub const SYNC: u8 = 0x05;
    pub const RESPONSES: u8 = 0x06;
    pub const ADD: u8 = 0x07;
    pub const CHANGE: u8 = 0x08;
    pub const DELETE: u8 = 0x09;
    pub const FETCH: u8 = 0x0A;
    pub const SYNC_KEY: u8 = 0x0B;
    pub const CLIENT_ID: u8 = 0x0C;
    pub const SERVER_ID: u8 = 0x0D;
    pub const STATUS: u8 = 0x0E;
    pub const COLLECTION: u8 = 0x0F;
    pub const COLLECTIONS: u8 = 0x1C;
    pub const CLASS: u8 = 0x10;
    pub const COLLECTION_ID: u8 = 0x12;
    pub const GET_CHANGES: u8 = 0x13;
    pub const MORE_AVAILABLE: u8 = 0x14;
    pub const WINDOW_SIZE: u8 = 0x15;
    pub const COMMANDS: u8 = 0x16;
    pub const OPTIONS: u8 = 0x17;
    pub const APPLICATION_DATA: u8 = 0x1D;
}

/// FolderHierarchy (page 7) tag ids.
pub mod folder {
    pub const FOLDERS: u8 = 0x05;
    pub const FOLDER: u8 = 0x06;
    pub const DISPLAY_NAME: u8 = 0x07;
    pub const SERVER_ID: u8 = 0x08;
    pub const PARENT_ID: u8 = 0x09;
    pub const TYPE: u8 = 0x0A;
    pub const STATUS: u8 = 0x0C;
    pub const CHANGES: u8 = 0x0E;
    pub const ADD: u8 = 0x0F;
    pub const DELETE: u8 = 0x10;
    pub const UPDATE: u8 = 0x11;
    pub const SYNC_KEY: u8 = 0x12;
    pub const FOLDER_CREATE: u8 = 0x13;
    pub const FOLDER_DELETE: u8 = 0x14;
    pub const FOLDER_UPDATE: u8 = 0x15;
    pub const FOLDER_SYNC: u8 = 0x16;
    pub const COUNT: u8 = 0x17;
}

/// Ping (page 13) tag ids.
pub mod ping {
    pub const PING: u8 = 0x05;
    pub const STATUS: u8 = 0x07;
    pub const HEARTBEAT_INTERVAL: u8 = 0x08;
    pub const FOLDERS: u8 = 0x09;
    pub const FOLDER: u8 = 0x0A;
    pub const ID: u8 = 0x0B;
    pub const CLASS: u8 = 0x0C;
    pub const MAX_FOLDERS: u8 = 0x0D;
}

/// Provision (page 14) tag ids.
pub mod provision {
    pub const PROVISION: u8 = 0x05;
    pub const POLICIES: u8 = 0x06;
    pub const POLICY: u8 = 0x07;
    pub const POLICY_TYPE: u8 = 0x08;
    pub const POLICY_KEY: u8 = 0x09;
    pub const DATA: u8 = 0x0A;
    pub const STATUS: u8 = 0x0B;
    pub const REMOTE_WIPE: u8 = 0x0C;
    pub const EAS_PROVISION_DOC: u8 = 0x0D;
}

/// ResolveRecipients (page 10) tag ids. Source: [MS-ASWBXML] §2.1.2.1.11,
/// verified against `RECIPIENTS_TOKENS` in `code_pages.rs`.
pub mod recipients {
    pub const RESOLVE_RECIPIENTS: u8 = 0x05;
    pub const RESPONSE: u8 = 0x06;
    pub const STATUS: u8 = 0x07;
    pub const TYPE: u8 = 0x08;
    pub const RECIPIENT: u8 = 0x09;
    pub const DISPLAY_NAME: u8 = 0x0A;
    pub const EMAIL_ADDRESS: u8 = 0x0B;
    pub const CERTIFICATES: u8 = 0x0C;
    pub const CERTIFICATE: u8 = 0x0D;
    pub const MINI_CERTIFICATE: u8 = 0x0E;
    pub const OPTIONS: u8 = 0x0F;
    pub const TO: u8 = 0x10;
    pub const CERTIFICATE_RETRIEVAL: u8 = 0x11;
    pub const RECIPIENT_COUNT: u8 = 0x12;
    pub const MAX_CERTIFICATES: u8 = 0x13;
    pub const MAX_AMBIGUOUS_RECIPIENTS: u8 = 0x14;
    pub const CERTIFICATE_COUNT: u8 = 0x15;
    pub const AVAILABILITY: u8 = 0x16;
    pub const START_TIME: u8 = 0x17;
    pub const END_TIME: u8 = 0x18;
    pub const MERGED_FREE_BUSY: u8 = 0x19;
    pub const PICTURE: u8 = 0x1A;
    pub const MAX_SIZE: u8 = 0x1B;
    pub const DATA: u8 = 0x1C;
    pub const MAX_PICTURES: u8 = 0x1D;
}

/// ValidateCert (page 11) tag ids. Source: [MS-ASWBXML] §2.1.2.1.12,
/// verified against `VALIDATE_TOKENS` in `code_pages.rs`.
pub mod validatecert {
    pub const VALIDATE_CERT: u8 = 0x05;
    pub const CERTIFICATES: u8 = 0x06;
    pub const CERTIFICATE: u8 = 0x07;
    pub const CERTIFICATE_CHAIN: u8 = 0x08;
    pub const CHECK_CRL: u8 = 0x09;
    pub const STATUS: u8 = 0x0A;
}

/// Settings (page 18) tag ids.
pub mod settings {
    pub const SETTINGS: u8 = 0x05;
    pub const STATUS: u8 = 0x06;
    pub const GET: u8 = 0x07;
    pub const SET: u8 = 0x08;
    pub const OOF: u8 = 0x09;
    /// Oof child: 0 = disabled, 1 = global, 2 = time-based ([MS-ASCMD]
    /// §2.2.3.124; MUST be 2 when StartTime/EndTime are present).
    pub const OOF_STATE: u8 = 0x0A;
    pub const START_TIME: u8 = 0x0B;
    pub const END_TIME: u8 = 0x0C;
    pub const OOF_MESSAGE: u8 = 0x0D;
    /// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.14).
    pub const APPLIES_TO_INTERNAL: u8 = 0x0E;
    /// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.12).
    pub const APPLIES_TO_EXTERNAL_KNOWN: u8 = 0x0F;
    /// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.13).
    pub const APPLIES_TO_EXTERNAL_UNKNOWN: u8 = 0x10;
    /// OofMessage child: "1"/"0" string ([MS-ASCMD] §2.2.3.59).
    pub const ENABLED: u8 = 0x11;
    pub const REPLY_MESSAGE: u8 = 0x12;
    pub const BODY_TYPE: u8 = 0x13;
    pub const DEVICE_PASSWORD: u8 = 0x14;
    pub const PASSWORD: u8 = 0x15;
    pub const DEVICE_INFORMATION: u8 = 0x16;
    pub const MODEL: u8 = 0x17;
    pub const IMEI: u8 = 0x18;
    pub const FRIENDLY_NAME: u8 = 0x19;
    pub const OS: u8 = 0x1A;
    pub const OS_LANGUAGE: u8 = 0x1B;
    pub const PHONE_NUMBER: u8 = 0x1C;
    pub const USER_INFORMATION: u8 = 0x1D;
    pub const EMAIL_ADDRESSES: u8 = 0x1E;
    pub const SMTP_ADDRESS: u8 = 0x1F;
}

/// ItemOperations (page 20) tag ids.
pub mod item_operations {
    pub const ITEM_OPERATIONS: u8 = 0x05;
    pub const FETCH: u8 = 0x06;
    pub const STORE: u8 = 0x07;
    pub const OPTIONS: u8 = 0x08;
    pub const RANGE: u8 = 0x09;
    pub const TOTAL: u8 = 0x0A;
    pub const PROPERTIES: u8 = 0x0B;
    pub const DATA: u8 = 0x0C;
    pub const STATUS: u8 = 0x0D;
    pub const RESPONSE: u8 = 0x0E;
    /// Part element ([MS-ASCMD] §2.2.3.130): multipart responses only.
    /// Child of airsyncbase:Body (or Properties for document-library
    /// fetches); its integer text is the index of the MultiPartResponse
    /// part carrying the payload, replacing the inline Data element.
    pub const PART: u8 = 0x11;
    /// EmptyFolderContents request/response element ([MS-ASCMD] §4.14.4).
    pub const EMPTY_FOLDER_CONTENTS: u8 = 0x12;
    /// Child of Options: also delete the folder's subfolders.
    pub const DELETE_SUB_FOLDERS: u8 = 0x13;
    /// Move request/response element — conversation move ([MS-ASCMD]
    /// §4.25). NOT the MoveItems-page (5) Move token; this is the
    /// ItemOperations-namespace Move.
    pub const MOVE: u8 = 0x16;
    /// Destination folder id, child of Move.
    pub const DST_FLD_ID: u8 = 0x17;
    /// Conversation id, child of Move — opaque binary on the wire.
    pub const CONVERSATION_ID: u8 = 0x18;
    /// Child of Options inside Move: also move all FUTURE messages of the
    /// conversation.
    pub const MOVE_ALWAYS: u8 = 0x19;
}

/// ComposeMail (page 21) tag ids.
pub mod compose {
    pub const SEND_MAIL: u8 = 0x05;
    pub const SMART_FORWARD: u8 = 0x06;
    pub const SMART_REPLY: u8 = 0x07;
    pub const SAVE_IN_SENT_ITEMS: u8 = 0x08;
    pub const REPLACE_MIME: u8 = 0x09;
    pub const SOURCE: u8 = 0x0B;
    pub const FOLDER_ID: u8 = 0x0C;
    pub const ITEM_ID: u8 = 0x0D;
    pub const LONG_ID: u8 = 0x0E;
    pub const INSTANCE_ID: u8 = 0x0F;
    pub const MIME: u8 = 0x10;
    pub const CLIENT_ID: u8 = 0x11;
    pub const STATUS: u8 = 0x12;
    pub const ACCOUNT_ID: u8 = 0x13;
}

/// Search (page 15) tag ids.
pub mod search {
    pub const PAGE: u8 = 15;
    pub const SEARCH: u8 = 0x05;
    pub const STORE: u8 = 0x07;
    pub const NAME: u8 = 0x08;
    pub const QUERY: u8 = 0x09;
    pub const OPTIONS: u8 = 0x0A;
    pub const RANGE: u8 = 0x0B;
    pub const STATUS: u8 = 0x0C;
    pub const RESPONSE: u8 = 0x0D;
    pub const RESULT: u8 = 0x0E;
    pub const PROPERTIES: u8 = 0x0F;
    pub const TOTAL: u8 = 0x10;
    pub const AND: u8 = 0x13;
    pub const FREE_TEXT: u8 = 0x15;
    pub const DEEP_TRAVERSAL: u8 = 0x17;
    pub const LONG_ID: u8 = 0x18;
    pub const REBUILD_RESULTS: u8 = 0x19;
}

/// GAL (page 16) tag ids.
pub mod gal {
    pub const PAGE: u8 = 16;
    pub const DISPLAY_NAME: u8 = 0x05;
    pub const PHONE: u8 = 0x06;
    pub const OFFICE: u8 = 0x07;
    pub const TITLE: u8 = 0x08;
    pub const COMPANY: u8 = 0x09;
    pub const ALIAS: u8 = 0x0A;
    pub const FIRST_NAME: u8 = 0x0B;
    pub const LAST_NAME: u8 = 0x0C;
    pub const HOME_PHONE: u8 = 0x0D;
    pub const MOBILE_PHONE: u8 = 0x0E;
    pub const EMAIL_ADDRESS: u8 = 0x0F;
}

/// AirSyncBase (page 17) tag ids.
pub mod base {
    pub const BODY_PREFERENCE: u8 = 0x05;
    pub const TYPE: u8 = 0x06;
    pub const TRUNCATION_SIZE: u8 = 0x07;
    pub const ALL_OR_NONE: u8 = 0x08;
    pub const BODY: u8 = 0x0A;
    pub const DATA: u8 = 0x0B;
    pub const ESTIMATED_DATA_SIZE: u8 = 0x0C;
    pub const TRUNCATED: u8 = 0x0D;
    pub const ATTACHMENTS: u8 = 0x0E;
    pub const ATTACHMENT: u8 = 0x0F;
    pub const DISPLAY_NAME: u8 = 0x10;
    pub const FILE_REFERENCE: u8 = 0x11;
    pub const METHOD: u8 = 0x12;
    pub const CONTENT_ID: u8 = 0x13;
    pub const CONTENT_LOCATION: u8 = 0x14;
    pub const IS_INLINE: u8 = 0x15;
    pub const NATIVE_BODY_TYPE: u8 = 0x16;
    pub const CONTENT_TYPE: u8 = 0x17;
    pub const PREVIEW: u8 = 0x18;
    /// `Location` = 0x20 (16.0/16.1 only; [MS-ASWBXML] §2.1.2.1.18 note —
    /// with 16.0/16.1 `airsyncbase:Location` replaces `calendar:Location`
    /// (4, 0x17)). CONTAINER type per [MS-ASAIRS] §2.2.2.28: the
    /// human-readable value is the `DisplayName` child (0x10 above).
    /// Registered here (M8-L1 variant) because both the Calendar
    /// ApplicationData parse and the Email MeetingRequest parse read it —
    /// see `calendar::parse_location_16x`.
    pub const LOCATION: u8 = 0x20;
}

/// Email (page 2) tag ids. Source: [MS-ASEMAIL] 2.2.2.
/// Used by the Sync-response parser to extract well-known email fields
/// out of `ApplicationData`.
pub mod email {
    pub const PAGE: u8 = 2;
    pub const DATE_RECEIVED: u8 = 0x0F;
    pub const SUBJECT: u8 = 0x14;
    pub const READ: u8 = 0x15;
    pub const TO: u8 = 0x16;
    pub const CC: u8 = 0x17;
    pub const FROM: u8 = 0x18;
    pub const REPLY_TO: u8 = 0x19;
    pub const IMPORTANCE: u8 = 0x12;
    pub const FLAG: u8 = 0x3A;
    // ---- Task 4: meeting-request tokens ([MS-ASEMAIL] §2.2.2) ----
    /// Outlook/Exchange message class (`IPM.Note`,
    /// `IPM.Schedule.Meeting.Request`, …). Drives the reading pane's
    /// meeting banner.
    pub const MESSAGE_CLASS: u8 = 0x13;
    /// MeetingRequest child: `"1"`/`"0"` boolean.
    pub const ALL_DAY_EVENT: u8 = 0x1A;
    /// MeetingRequest child (xs:dateTime).
    pub const END_TIME: u8 = 0x1E;
    /// MeetingRequest child: 0=single, 1=master recurring, 2=exception
    /// instance, 3=exception master ([MS-ASEMAIL] §2.2.2.36).
    pub const INSTANCE_TYPE: u8 = 0x1F;
    /// MeetingRequest child.
    pub const LOCATION: u8 = 0x21;
    /// Container for the meeting logistics children above
    /// ([MS-ASEMAIL] §2.2.2.48).
    pub const MEETING_REQUEST: u8 = 0x22;
    /// MeetingRequest child (organizer's SMTP address).
    pub const ORGANIZER: u8 = 0x23;
    /// MeetingRequest child: `"1"`/`"0"` boolean.
    pub const RESPONSE_REQUESTED: u8 = 0x26;
    /// MeetingRequest child (xs:dateTime).
    pub const START_TIME: u8 = 0x31;
    /// MeetingRequest child (≤14.1): base64 GlobalObjectId — converted to
    /// the calendar UID string per [MS-ASEMAIL] §3.1.4.7 before joining
    /// against a calendar item ([MS-ASWBXML] §2.1.2.1.4 note 4: at 16.0/16.1
    /// the calendar-page UID replaces this element, same value space).
    pub const GLOBAL_OBJ_ID: u8 = 0x34;
}

/// Email2 (page 22) tag ids. Source: [MS-ASEMAIL] 2.2.3.
/// Conversations / drafts / BCC live here because they postdate the
/// original Email code page.
pub mod email2 {
    pub const PAGE: u8 = 22;
    pub const CONVERSATION_ID: u8 = 0x09;
    pub const IS_DRAFT: u8 = 0x15;
    pub const BCC: u8 = 0x16;
    /// [MS-ASEMAIL] §2.2.2.47 (v20220429): 0=silent update/unspecified,
    /// 1=initial meeting request, 2=full update, 3=informational update,
    /// 4=outdated, 5=delegator's copy. [MS-ASCMD] §3.1.5.6: only 1|2 (the
    /// initial request + the full update) arm the Accept/Tentative/Decline
    /// response UI. (An earlier comment here carried a value-off-by-one
    /// mapping; corrected against the spec table 2026-08-18 — the 1|2 gate
    /// itself was already right.)
    pub const MEETING_MESSAGE_TYPE: u8 = 0x13;
}
