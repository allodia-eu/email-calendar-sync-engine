// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Minimal EAS type set for MVP scope (9 commands: FolderSync, Sync, SendMail,
// SmartForward, SmartReply, ItemOperations, GetItemEstimate, Ping, FolderCreate/Delete/Update).
// Full type coverage (Provision, Settings, Search, ResolveRecipients, ValidateCert,
// Find, AutoDiscover, MeetingResponse) is deferred.

mod config;
mod folder;
mod item_operations;
mod mail;
mod ping;
mod recipients;
mod search;
mod settings;
mod sync;

pub use config::{EasConfig, EasServerOptions};
pub use folder::{
    EasFolder, FolderCreateRequest, FolderDeleteRequest, FolderSyncResult, FolderUpdateRequest,
};
pub use item_operations::{
    ConversationMoveRequest, ConversationMoveResult, EmptyFolderContentsRequest,
    EmptyFolderContentsResult, ItemOperationsFetchRequest, ItemOperationsFetchResult,
};
pub use mail::{
    CLIENT_ID_MAX_LEN, SendMailRequest, SmartForwardRequest, SmartReplyRequest,
    new_calendar_client_id, new_send_client_id,
};
pub use ping::{PingCollection, PingRequest, PingResult};
pub use recipients::{
    ResolveRecipientsRequest, ResolveRecipientsResponse, ResolveRecipientsResult, ResolvedRecipient,
};
pub use search::{GalEntry, SearchRequest, SearchResult, SearchResultItem};
pub use settings::{
    DevicePasswordResult, OofAppliesTo, OofMessage, OofResult, OofSettings, UserInformationResult,
    ValidateCertRequest, ValidateCertResult,
};
pub use sync::{
    CalendarItemWithId, ContactsItemWithId, EasAttachment, EasItem, GetItemEstimateRequest,
    GetItemEstimateResult, MeetingRequestInfo, SupportedElement, SyncRequest, SyncResult,
};
