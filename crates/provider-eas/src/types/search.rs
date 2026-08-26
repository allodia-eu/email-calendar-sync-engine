// SPDX-License-Identifier: MPL-2.0
//! Search / GAL request-response types.

use serde::{Deserialize, Serialize};

use super::EasItem;
// ---------- Search ----------

/// Search request ([MS-ASCMD] §2.2.1.17): a Mailbox or GAL query with a
/// paged result window.
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

/// One GAL directory entry from a Search response (the `gal:Properties`
/// block, [MS-ASGAL]).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GalEntry {
    /// `gal:DisplayName`, when present.
    pub display_name: Option<String>,
    /// `gal:Phone` (business), when present.
    pub phone: Option<String>,
    /// `gal:Office`, when present.
    pub office: Option<String>,
    /// `gal:Title`, when present.
    pub title: Option<String>,
    /// `gal:Company`, when present.
    pub company: Option<String>,
    /// `gal:Alias`, when present.
    pub alias: Option<String>,
    /// `gal:FirstName`, when present.
    pub first_name: Option<String>,
    /// `gal:LastName`, when present.
    pub last_name: Option<String>,
    /// `gal:HomePhone`, when present.
    pub home_phone: Option<String>,
    /// `gal:MobilePhone`, when present.
    pub mobile_phone: Option<String>,
    /// `gal:EmailAddress` (SMTP), when present.
    pub email_address: Option<String>,
}

/// One result row of a Search response: a Mailbox hit wraps an
/// [`EasItem`]; a GAL hit wraps a [`GalEntry`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResultItem {
    /// Result class (`"Email"` for Mailbox rows, `"GAL"` for directory
    /// rows), when present.
    pub class: Option<String>,
    /// `search:LongId` — the handle an ItemOperations LongId fetch uses,
    /// when present.
    pub long_id: Option<String>,
    /// `airsync:CollectionId` of the hit's folder (Mailbox rows), when
    /// present.
    pub collection_id: Option<String>,
    /// The Mailbox item properties, for Mailbox rows.
    #[serde(default)]
    pub item: Option<EasItem>,
    /// The GAL directory properties, for GAL rows.
    #[serde(default)]
    pub gal: Option<GalEntry>,
}

/// Result of the Search command: the status pair, the result window, and
/// the rows in that window.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResult {
    /// Command-level `search:Status` (1 = success).
    pub status: u32,
    /// Store-level `search:Status` (1 = success), when present.
    pub store_status: Option<u32>,
    /// The `"m-n"` window these results occupy, when the server echoed it.
    pub range: Option<String>,
    /// Total matches server-side (across all pages), when present.
    pub total: Option<u32>,
    /// Result rows, in wire order.
    pub results: Vec<SearchResultItem>,
}
