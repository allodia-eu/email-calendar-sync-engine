// SPDX-License-Identifier: MPL-2.0
// Sync request building (downsync envelope).

use crate::{
    commands::{
        AS_COLLECTION, AS_COLLECTION_ID, AS_COLLECTIONS, AS_DELETES_AS_MOVES, AS_FILTER_TYPE,
        AS_GET_CHANGES, AS_MIME_SUPPORT, AS_MIME_TRUNCATION, AS_OPTIONS, AS_SUPPORTED, AS_SYNC,
        AS_SYNC_KEY, AS_WINDOW_SIZE, PAGE_AIRSYNC, SyncRequest, WbxmlElement, tags,
    },
    wbxml::tags::pages,
};
// ============================================================================
// Sync
// ============================================================================

/// Build a Sync request for a single collection.
///
/// `Collection` children follow the strict order of [MS-ASCMD] §2.2.3.29.2:
/// SyncKey, CollectionId, (Supported), DeletesAsMoves, GetChanges,
/// WindowSize, (ConversationMode), Options, Commands — this builder emits
/// `Supported` when [`SyncRequest::supported`] is `Some(non-empty)` and
/// never emits ConversationMode/Commands.
///
/// `<DeletesAsMoves/>` is emitted unconditionally right after CollectionId
/// (or after `Supported` when that is present):
/// every [MS-ASCMD] §4.5 Sync example sends it, and the empty form means
/// TRUE (§2.2.3.43) — deletes move to the Deleted Items folder instead of
/// being hard-deleted, matching client behavior.
///
/// `protocol_version` gates the `GetChanges` element: per [MS-ASSYNC]
/// §2.2.2.9 it is not valid in 16.1 (the server sends changes by default and
/// rejects requests carrying it — live evidence: eas_sync_bisect against
/// Exchange 2019, every GetChanges variant answered top-level Status=4).
/// Pre-16.1 it is required, so it is emitted for any other version string.
///
/// `Options` is emitted when a `FilterType` (`filter_age_days != 0`), a
/// `BodyPreference` (`fetch_body`), or a MIME option (`mime_support` /
/// `mime_truncation` `Some`) is requested, with FilterType as the FIRST
/// child ([MS-ASCMD] §2.2.3.125.6). `MIMESupport` / `MIMETruncation` follow
/// the BodyPreference — the §2.2.3.125.6 Options child order: FilterType?,
/// Class?, ConversationMode?, MaxItems?, BodyPreference*, MIMESupport?,
/// MIMETruncation?, RightsManagementSupport?.
pub fn build_sync_request(req: &SyncRequest, protocol_version: &str) -> WbxmlElement {
    let mut collection_children = vec![
        WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, req.sync_key.clone()),
        WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, req.collection_id.clone()),
    ];

    // [MS-ASCMD] §2.2.3.179 / §2.2.3.29.2: `Supported` names the schema
    // elements the client supports for this collection class (ghosting
    // control for future editing) and sits between CollectionId and
    // DeletesAsMoves. Each entry is emitted as an empty tag — the §4.24
    // shape. Only Some(non-empty) emits: None/Some([]) keep the request
    // byte-identical to the pre-Supported shape (per rule 1 an absent
    // Supported ghosts nothing; the wire-level `<Supported/>` "ghost
    // everything" form of rule 3 is deliberately unreachable).
    if let Some(supported) = req.supported.as_deref().filter(|s| !s.is_empty()) {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_SUPPORTED,
            supported
                .iter()
                .map(|e| WbxmlElement::empty(e.page, e.token))
                .collect(),
        ));
    }

    // [MS-ASCMD] §2.2.3.29.2 order: DeletesAsMoves directly after
    // CollectionId (or Supported, when emitted above). Empty element =
    // TRUE (§2.2.3.43): server-side deletes go to Deleted Items, not away.
    collection_children.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_DELETES_AS_MOVES));

    if protocol_version != "16.1" {
        collection_children.push(WbxmlElement::empty(PAGE_AIRSYNC, AS_GET_CHANGES));
    }

    if req.window_size != 0 {
        collection_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_WINDOW_SIZE,
            req.window_size.to_string(),
        ));
    }

    // Per [MS-ASSYNC] 2.2.3.25 / [MS-ASCMD] §2.2.3.125.6 — `Options` inside a
    // `Collection` controls how the server synchronizes it. Child order per
    // the Options (Sync) schema: FilterType FIRST, then BodyPreference.
    let mut options_children: Vec<WbxmlElement> = Vec::new();

    // FilterType (page 0, 0x18) bounds the sync to a time window
    // ([MS-ASCMD] §2.2.3.68.2; 0 = no filter, hence omitted then). Mirrors
    // `build_get_item_estimate_request`, which already sends it. Emitted
    // ahead of BodyPreference and even when `fetch_body` is false, so
    // header-only rounds honor the same age window (sticky-options note in
    // §2.2.3.125.6 makes the explicit block important: an omitted Options
    // reuses the PREVIOUS block).
    if req.filter_age_days != 0 {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_FILTER_TYPE,
            req.filter_age_days.to_string(),
        ));
    }

    // AirSyncBase `BodyPreference` with `Type=2` (HTML) so the server returns
    // message bodies. Gated on `fetch_body` so header-only sync rounds stay
    // cheap. Code-page ids: AirSyncBase = 17 (pages::BASE); tokens are
    // `BodyPreference` (0x05) and `Type` (0x06) per tags::base.
    //
    // When `truncation_size` is set, a `TruncationSize` child (token 0x07 —
    // verified against MS-ASWBXML.txt §2.1.2.1.18, AirSyncBase page 17 table)
    // follows `Type`, capping the per-item body payload the server returns
    // (children order per MS-ASAIRS BodyPreference: Type, TruncationSize,
    // AllOrNone). Larger bodies come back `Truncated=1` and are completed
    // on demand via ItemOperations (`fetch_body`).
    if req.fetch_body {
        let mut pref_children = vec![WbxmlElement::text(pages::BASE, tags::base::TYPE, "2")];
        if let Some(truncation_size) = req.truncation_size {
            pref_children.push(WbxmlElement::text(
                pages::BASE,
                tags::base::TRUNCATION_SIZE,
                truncation_size.to_string(),
            ));
        }
        // MS-ASAIRS 2.2.2.35.4: the server only returns `Body > Preview`
        // (the message-list snippet) when the BodyPreference carries a
        // Preview child (0-255 = max chars). Schema order keeps it LAST
        // (Type, TruncationSize, AllOrNone, Preview). Without it every
        // synced message had an empty snippet (live finding 2026-08-04).
        pref_children.push(WbxmlElement::text(pages::BASE, tags::base::PREVIEW, "255"));
        options_children.push(WbxmlElement::container(
            pages::BASE,
            tags::base::BODY_PREFERENCE,
            pref_children,
        ));
    }

    // MIMESupport / MIMETruncation ([MS-ASCMD] §2.2.3.110.3 / §2.2.3.111)
    // follow BodyPreference per the §2.2.3.125.6 Options child order. Both
    // are page-0 AirSync tokens (0x22 / 0x23). Emitted only when Some —
    // None keeps the request byte-for-byte identical to the pre-MIME shape
    // (an absent MIMESupport reads as 0 = never send MIME on the server).
    if let Some(level) = req.mime_support {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_MIME_SUPPORT,
            level.to_string(),
        ));
    }
    if let Some(level) = req.mime_truncation {
        options_children.push(WbxmlElement::text(
            PAGE_AIRSYNC,
            AS_MIME_TRUNCATION,
            level.to_string(),
        ));
    }

    if !options_children.is_empty() {
        collection_children.push(WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_OPTIONS,
            options_children,
        ));
    }

    // NOTE: no `airsync:Class` element is emitted. Per [MS-ASSYNC] §2.2.2.11
    // Class is only a valid `Collection` child in protocol 2.5/12.x; in 14.0+
    // `CollectionId` identifies the collection, and Exchange 16.1 rejects a
    // request carrying Class with top-level Status=4 ("<Class> ... appears
    // out of order" — live evidence: eas_sync_debug raw dump, 2026-08-02).
    let collection = WbxmlElement::container(PAGE_AIRSYNC, AS_COLLECTION, collection_children);

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}
