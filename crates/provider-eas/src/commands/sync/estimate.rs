// SPDX-License-Identifier: MPL-2.0
// GetItemEstimate request/response marshalers.

use crate::commands::{
    AS_FILTER_TYPE, AS_OPTIONS, AS_SYNC_KEY, GetItemEstimateRequest, GetItemEstimateResult,
    PAGE_AIRSYNC, WbxmlElement, WbxmlError, text_value,
};
// ============================================================================
// GetItemEstimate
// ============================================================================

/// Build a GetItemEstimate request.
///
/// Wire shape per [MS-ASWBXML] §2.1.2.1.7 (code page 6) and the [MS-ASCMD]
/// §6.20 request schema, 14.0+ form A:
/// ```xml
/// <GetItemEstimate>                          <!-- page 6, 0x05 -->
///   <Collections>                            <!-- page 6, 0x07 -->
///     <Collection>                           <!-- page 6, 0x08 -->
///       <airsync:SyncKey>{sync_key}</>       <!-- PAGE 0, 0x0B — not page 6 -->
///       <CollectionId>{collection_id}</>     <!-- page 6, 0x0A (0x0C is Estimate!) -->
///       <airsync:Options>                    <!-- PAGE 0, 0x17; only when filtering -->
///         <airsync:FilterType>{days}</>      <!-- PAGE 0, 0x18 — not page 6 -->
///       </airsync:Options>
///     </Collection>
///   </Collections>
/// </GetItemEstimate>
/// ```
/// Notes:
/// - SyncKey / FilterType are AirSync-page (0) tokens even inside a GetItemEstimate request
///   ([MS-ASCMD] §2.2.1.9 element table prefixes them `airsync:`; the page-6 token table has no
///   SyncKey/FilterType at all).
/// - There is no top-level `Class` element in the 14.0+ request form. The page-6 Class token (0x09)
///   is 2.5/12.x-only per [MS-ASWBXML] §2.1.2.1.7 note 1, and the §6.20 form-A sequence has no
///   Class slot — so `req.class` is intentionally not emitted (the client negotiates 16.1).
/// - FilterType 0 means "all items" (the server default), so the whole Options element is omitted
///   when `filter_age_days == 0`.
pub fn build_get_item_estimate_request(req: &GetItemEstimateRequest) -> WbxmlElement {
    pub(crate) const PAGE_GIE: u8 = 6;
    pub(crate) const GIE_GET_ITEM_ESTIMATE: u8 = 0x05;
    pub(crate) const GIE_COLLECTIONS: u8 = 0x07;
    pub(crate) const GIE_COLLECTION: u8 = 0x08;
    pub(crate) const GIE_COLLECTION_ID: u8 = 0x0A;

    let mut collection_children = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, req.sync_key.clone()),
        WbxmlElement::text(PAGE_GIE, GIE_COLLECTION_ID, req.collection_id.clone()),
    ];
    if req.filter_age_days != 0 {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_OPTIONS,
            vec![WbxmlElement::text(
                PAGE_AIRSYNC,
                AS_FILTER_TYPE,
                req.filter_age_days.to_string(),
            )],
        ));
    }

    let collection = WbxmlElement::container(PAGE_GIE, GIE_COLLECTION, collection_children);

    WbxmlElement::container(
        PAGE_GIE,
        GIE_GET_ITEM_ESTIMATE,
        vec![WbxmlElement::container(
            PAGE_GIE,
            GIE_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Parse a GetItemEstimate response per [MS-ASCMD] §6.21:
/// ```xml
/// <GetItemEstimate>            <!-- page 6, 0x05 -->
///   <Response>                 <!-- page 6, 0x0D (NOT 0x06) -->
///     <Status>1</Status>       <!-- page 6, 0x0E — sibling of Collection -->
///     <Collection>             <!-- page 6, 0x08 -->
///       <CollectionId>..</>    <!-- page 6, 0x0A -->
///       <Estimate>42</>        <!-- page 6, 0x0C -->
///     </Collection>
///   </Response>
/// </GetItemEstimate>
/// ```
/// The Response-level `<Status>` is surfaced on `result.status` (1 = success,
/// 3 = sync state not primed); an absent Status defaults to 1. Live evidence
/// 2026-08-02: Exchange 2019 answers Status 3 for a collection that has never
/// been Synced — callers must read it, not assume count-0 means "up to date".
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_get_item_estimate_response(
    root: &WbxmlElement,
) -> Result<GetItemEstimateResult, WbxmlError> {
    pub(crate) const PAGE_GIE: u8 = 6;
    pub(crate) const GIE_RESPONSE: u8 = 0x0D;
    pub(crate) const GIE_STATUS: u8 = 0x0E;
    pub(crate) const GIE_COLLECTION: u8 = 0x08;
    pub(crate) const GIE_COLLECTION_ID: u8 = 0x0A;
    pub(crate) const GIE_ESTIMATE: u8 = 0x0C;

    let mut result = GetItemEstimateResult {
        status: 1, // success default per [MS-ASCMD] §6.21 when Status absent
        ..GetItemEstimateResult::default()
    };
    for child in &root.children {
        if child.page == PAGE_GIE && child.token == GIE_RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == PAGE_GIE && resp_child.token == GIE_STATUS {
                    let s = text_value(resp_child).unwrap_or_default();
                    result.status = s.parse().unwrap_or(1);
                } else if resp_child.page == PAGE_GIE && resp_child.token == GIE_COLLECTION {
                    for col_child in &resp_child.children {
                        match (col_child.page, col_child.token) {
                            (PAGE_GIE, GIE_COLLECTION_ID) => {
                                result.collection_id = text_value(col_child).unwrap_or_default();
                            }
                            (PAGE_GIE, GIE_ESTIMATE) => {
                                let s = text_value(col_child).unwrap_or("0".to_string());
                                result.count = s.parse().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
