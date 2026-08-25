// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// MS-ASWBXML code page tables. The tables here mirror the `addXxxTokens`
// helpers in `wbxml_helper.ets`, which are the authoritative source (the
// `tags.ts mPages` array contains duplicate / stub entries and is not used
// at runtime). Each entry is `(token_id, tag_name)`; the token id is what
// appears on the wire after masking off the content/attribute bits.

use crate::wbxml::global_tokens::is_global_token;

/// Number of MS-ASWBXML code pages (0..=25).
pub const NUM_CODE_PAGES: usize = 26;

/// A single MS-ASWBXML code page.
#[derive(Debug, Clone)]
pub struct CodePage {
    /// Human-readable namespace name (e.g. `"AirSync"`).
    pub namespace: &'static str,
    /// XML namespace prefix used in the decoded document (e.g. `"airsync"`).
    pub xmlns: &'static str,
    /// Sorted `(token_id, tag_name)` pairs for this page.
    pub tokens: &'static [(u8, &'static str)],
}

impl CodePage {
    /// Look up a tag name by token id. Returns `None` if not registered.
    pub fn tag_name(&self, token: u8) -> Option<&'static str> {
        // Linear scan — these tables are tiny (< 60 entries).
        self.tokens
            .iter()
            .find(|(t, _)| *t == token)
            .map(|(_, n)| *n)
    }

    /// Look up a token id by tag name. Comparison is case-insensitive
    /// (matches the ArkTS `getCodePageByNamespace` + `getToken` behavior).
    /// Returns `None` if not registered.
    pub fn token(&self, tag_name: &str) -> Option<u8> {
        let needle = tag_name.to_ascii_lowercase();
        self.tokens
            .iter()
            .find(|(_, n)| n.to_ascii_lowercase() == needle)
            .map(|(t, _)| *t)
    }
}

/// Code pages 0–9: AirSync, Contacts, Email, AirNotify, Calendar, Move,
/// GetItemEstimate, FolderHierarchy, MeetingResponse, Tasks.
mod pages_00_09;
/// Code pages 10–17: ResolveRecipients, ValidateCert, Contacts2, Ping,
/// Provision, Search, GAL, AirSyncBase.
mod pages_10_17;
/// Code pages 18–25: Settings, DocumentLibrary, ItemOperations, ComposeMail,
/// Email2, Notes, RightsManagement, Find.
mod pages_18_25;

use pages_00_09::{
    AIRNOTIFY_TOKENS, AIRSYNC_TOKENS, CALENDAR_TOKENS, CONTACTS_TOKENS, EMAIL_TOKENS,
    FOLDER_TOKENS, GIE_TOKENS, MOVE_TOKENS, MREQ_TOKENS, TASK_TOKENS,
};
use pages_10_17::{
    BASE_TOKENS, CONTACTS2_TOKENS, GAL_TOKENS, PING_TOKENS, PROVISION_TOKENS, RECIPIENTS_TOKENS,
    SEARCH_TOKENS, VALIDATE_TOKENS,
};
use pages_18_25::{
    COMPOSE_TOKENS, DOCS_TOKENS, EMAIL2_TOKENS, FIND_TOKENS, ITEMS_TOKENS, NOTES_TOKENS,
    RIGHTS_TOKENS, SETTINGS_TOKENS,
};

/// All 26 MS-ASWBXML code pages, indexed by page number.
pub static CODE_PAGES: [CodePage; NUM_CODE_PAGES] = [
    CodePage {
        namespace: "AirSync",
        xmlns: "airsync",
        tokens: AIRSYNC_TOKENS,
    },
    CodePage {
        namespace: "Contacts",
        xmlns: "contacts",
        tokens: CONTACTS_TOKENS,
    },
    CodePage {
        namespace: "Email",
        xmlns: "email",
        tokens: EMAIL_TOKENS,
    },
    CodePage {
        namespace: "",
        xmlns: "",
        tokens: AIRNOTIFY_TOKENS,
    },
    CodePage {
        namespace: "Calendar",
        xmlns: "calendar",
        tokens: CALENDAR_TOKENS,
    },
    CodePage {
        namespace: "Move",
        xmlns: "move",
        tokens: MOVE_TOKENS,
    },
    CodePage {
        namespace: "GetItemEstimate",
        xmlns: "getitemestimate",
        tokens: GIE_TOKENS,
    },
    CodePage {
        namespace: "FolderHierarchy",
        xmlns: "folderhierarchy",
        tokens: FOLDER_TOKENS,
    },
    CodePage {
        namespace: "MeetingResponse",
        xmlns: "meetingresponse",
        tokens: MREQ_TOKENS,
    },
    CodePage {
        namespace: "Tasks",
        xmlns: "tasks",
        tokens: TASK_TOKENS,
    },
    CodePage {
        namespace: "ResolveRecipients",
        xmlns: "resolverecipients",
        tokens: RECIPIENTS_TOKENS,
    },
    CodePage {
        namespace: "ValidateCert",
        xmlns: "ValidateCert",
        tokens: VALIDATE_TOKENS,
    },
    CodePage {
        namespace: "Contacts2",
        xmlns: "contacts2",
        tokens: CONTACTS2_TOKENS,
    },
    CodePage {
        namespace: "Ping",
        xmlns: "ping",
        tokens: PING_TOKENS,
    },
    CodePage {
        namespace: "Provision",
        xmlns: "provision",
        tokens: PROVISION_TOKENS,
    },
    CodePage {
        namespace: "Search",
        xmlns: "search",
        tokens: SEARCH_TOKENS,
    },
    CodePage {
        namespace: "GAL",
        xmlns: "gal",
        tokens: GAL_TOKENS,
    },
    CodePage {
        namespace: "AirSyncBase",
        xmlns: "airsyncbase",
        tokens: BASE_TOKENS,
    },
    CodePage {
        namespace: "Settings",
        xmlns: "settings",
        tokens: SETTINGS_TOKENS,
    },
    CodePage {
        namespace: "DocumentLibrary",
        xmlns: "documentlibrary",
        tokens: DOCS_TOKENS,
    },
    CodePage {
        namespace: "ItemOperations",
        xmlns: "itemoperations",
        tokens: ITEMS_TOKENS,
    },
    CodePage {
        namespace: "ComposeMail",
        xmlns: "composemail",
        tokens: COMPOSE_TOKENS,
    },
    CodePage {
        namespace: "Email2",
        xmlns: "email2",
        tokens: EMAIL2_TOKENS,
    },
    CodePage {
        namespace: "Notes",
        xmlns: "notes",
        tokens: NOTES_TOKENS,
    },
    CodePage {
        namespace: "RightsManagement",
        xmlns: "rightsmanagement",
        tokens: RIGHTS_TOKENS,
    },
    CodePage {
        namespace: "Find",
        xmlns: "find",
        tokens: FIND_TOKENS,
    },
];

/// Return the code page for `page`, or `None` if out of range.
pub fn code_page(page: u8) -> Option<&'static CodePage> {
    CODE_PAGES.get(page as usize)
}

/// Return `true` if `page` is a known code page index.
pub fn is_valid_page(page: u8) -> bool {
    (page as usize) < NUM_CODE_PAGES
}

/// Return `true` if `page` is known and has a registered token `token`.
pub fn is_valid_tag(page: u8, token: u8) -> bool {
    match code_page(page) {
        Some(p) => p.tag_name(token).is_some(),
        None => false,
    }
}

/// Return `true` if `token` falls inside the global-token range
/// (`0x00..=0x04` — SWITCH_PAGE, END, ENTITY, STR_I, LITERAL).
pub fn is_global_tag(token: u8) -> bool {
    is_global_token(token)
}
