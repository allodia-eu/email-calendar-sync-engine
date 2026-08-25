// SPDX-License-Identifier: MPL-2.0
//! build_sync_request Options: FilterType ordering, default shape, MIME elements,
//! airsync:Supported.

use super::*;

/// When `filter_age_days != 0` and bodies are fetched, Options must carry
/// `FilterType` as its FIRST child with BodyPreference after it
/// (§2.2.3.125.6 / task-brief Options order).
#[test]
fn build_sync_request_emits_filter_type_first_in_options_with_body_preference() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 3,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        options.children.len(),
        2,
        "Options must hold exactly FilterType + BodyPreference, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );

    let filter = &options.children[0];
    assert_eq!(
        (filter.page, filter.token),
        (PAGE_AIRSYNC, 0x18),
        "FilterType must be the FIRST Options child (page 0, 0x18)"
    );
    assert_eq!(filter.tag_name(), "FilterType");
    assert_eq!(filter.value, WbxmlValue::Text("3".to_string()));

    let body_pref = &options.children[1];
    assert_eq!(
        (body_pref.page, body_pref.token),
        (pages::BASE, base::BODY_PREFERENCE),
        "BodyPreference must follow FilterType inside Options"
    );
}

/// `fetch_body: false` with `filter_age_days != 0`: Options must STILL
/// be emitted — with ONLY FilterType — so the age filter applies to
/// header-only sync rounds too.
#[test]
fn build_sync_request_emits_options_with_only_filter_type_when_fetch_body_false() {
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("Options must be emitted when filter_age_days != 0 even with fetch_body=false");
    assert_eq!(
        options.children.len(),
        1,
        "Options must contain ONLY FilterType when fetch_body=false, got {:?}",
        options
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>()
    );
    let filter = &options.children[0];
    assert_eq!((filter.page, filter.token), (PAGE_AIRSYNC, 0x18));
    assert_eq!(filter.value, WbxmlValue::Text("7".to_string()));
}

/// Default production shape (engine drain loop: `filter_age_days: 0`,
/// `fetch_body: true`): DeletesAsMoves is present (unconditional), but
/// FilterType stays omitted and Options keeps its single BodyPreference
/// child — no wire regression beyond the new DeletesAsMoves element.
#[test]
fn build_sync_request_default_shape_has_deletes_as_moves_and_no_filter_type() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 100,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let has_deletes_as_moves = collection
        .children
        .iter()
        .any(|c| c.page == PAGE_AIRSYNC && c.token == 0x1E);
    assert!(
        has_deletes_as_moves,
        "DeletesAsMoves is unconditional — spec examples always send it"
    );

    let has_filter_type = collection.children.iter().any(|c| {
        (c.page == PAGE_AIRSYNC && c.token == 0x18)
            || (c.page == PAGE_AIRSYNC
                && c.token == AS_OPTIONS
                && c.children
                    .iter()
                    .any(|o| o.page == PAGE_AIRSYNC && o.token == 0x18))
    });
    assert!(
        !has_filter_type,
        "FilterType must NOT be emitted when filter_age_days=0 (0 = no filter, §2.2.3.68.2)"
    );

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        options.children.len(),
        1,
        "Options keeps its single BodyPreference child for filter_age_days=0"
    );
    assert_eq!(
        (options.children[0].page, options.children[0].token),
        (pages::BASE, base::BODY_PREFERENCE)
    );
}

// ---- Task 3 (eas-p2-polish): MIMESupport + MIMETruncation in Sync Options ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §2.2.3.125.6 Options (Sync) child set: FilterType, …, BodyPreference*, MIMESupport?,
//   MIMETruncation?, … — MIMESupport/MIMETruncation go AFTER BodyPreference (task-brief order:
//   FilterType?, Class?, ConversationMode?, MaxItems?, BodyPreference*, MIMESupport?,
//   MIMETruncation?, RightsManagementSupport?).
// - §2.2.3.110.3 MIMESupport (Sync): 0 = never send MIME, 1 = S/MIME messages only, 2 = all
//   messages; absent defaults to 0.
// - §2.2.3.111 MIMETruncation: levels 0-8 (0 = truncate all … 8 = send complete MIME data).
// - Both are AirSync-page (0) tokens: MIMESupport 0x22, MIMETruncation 0x23 (verified in
//   code_pages.rs AIRSYNC_TOKENS).

/// With a filter AND bodies, Options children must be exactly
/// [FilterType, BodyPreference, MIMESupport, MIMETruncation] in that
/// order — MIMESupport/MIMETruncation follow BodyPreference
/// (§2.2.3.125.6).
#[test]
fn build_sync_request_emits_mime_elements_after_body_preference_with_filter() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: true,
        truncation_size: None,
        mime_support: Some(1),
        mime_truncation: Some(4),
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![
            (PAGE_AIRSYNC, 0x18), // FilterType
            (pages::BASE, base::BODY_PREFERENCE),
            (PAGE_AIRSYNC, 0x22), // MIMESupport
            (PAGE_AIRSYNC, 0x23), // MIMETruncation
        ],
        "Options order must be FilterType, BodyPreference, MIMESupport, MIMETruncation (§2.2.3.125.6)"
    );

    let mime_support = &options.children[2];
    assert_eq!(mime_support.tag_name(), "MIMESupport");
    assert_eq!(mime_support.value, WbxmlValue::Text("1".to_string()));

    let mime_truncation = &options.children[3];
    assert_eq!(mime_truncation.tag_name(), "MIMETruncation");
    assert_eq!(mime_truncation.value, WbxmlValue::Text("4".to_string()));
}

/// Without a filter (the production default), Options children must be
/// exactly [BodyPreference, MIMESupport, MIMETruncation].
#[test]
fn build_sync_request_emits_mime_elements_after_body_preference_without_filter() {
    use provider_eas::wbxml::tags::{base, pages};

    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: Some(1),
        mime_truncation: Some(4),
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");

    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options element inside Collection");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![
            (pages::BASE, base::BODY_PREFERENCE),
            (PAGE_AIRSYNC, 0x22), // MIMESupport
            (PAGE_AIRSYNC, 0x23), // MIMETruncation
        ],
        "without FilterType, MIMESupport/MIMETruncation still follow BodyPreference"
    );
}

/// `mime_support: None` + `mime_truncation: None` must keep the request
/// byte-identical to the pre-Task-3 shape: Options holds exactly its
/// previous children ([BodyPreference] when bodies are fetched,
/// [FilterType] for header-only filtered rounds) and no MIMESupport /
/// MIMETruncation token appears anywhere.
#[test]
fn build_sync_request_omits_mime_elements_when_fields_none() {
    use provider_eas::wbxml::tags::{base, pages};

    // Shape 1: production default (no filter, bodies on).
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 100,
        filter_age_days: 0,
        fetch_body: true,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");
    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![(pages::BASE, base::BODY_PREFERENCE)],
        "Options keeps its single BodyPreference child when mime fields are None"
    );

    // Shape 2: header-only filtered round.
    let req = SyncRequest {
        collection_id: "22".to_string(),
        sync_key: "0".to_string(),
        class: "Email".to_string(),
        window_size: 25,
        filter_age_days: 7,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: None,
    };
    let collection = sync_collection_for(&req, "16.1");
    let options = collection
        .children
        .iter()
        .find(|c| c.page == PAGE_AIRSYNC && c.token == AS_OPTIONS)
        .expect("missing Options");
    assert_eq!(
        collection_child_tokens_of_options(options),
        vec![(PAGE_AIRSYNC, 0x18)],
        "Options keeps its single FilterType child when mime fields are None"
    );
}

/// `(page, token)` sequence of an Options element's children, for
/// exact-order assertions (Options-level sibling of
/// `collection_child_tokens`).
fn collection_child_tokens_of_options(options: &WbxmlElement) -> Vec<(u8, u8)> {
    options.children.iter().map(|c| (c.page, c.token)).collect()
}

// ---- Task 2 (eas-p3-commands): airsync:Supported element ----
//
// Spec anchors (docs/Exchange/mscmd.txt, [MS-ASCMD] v20250520):
// - §2.2.3.179 Supported: optional Collection child in a Sync request naming the contact/calendar
//   elements the client manages; elements NOT named are "ghosted" — a later Change omitting a
//   ghosted element PRESERVES its server-side value instead of deleting it (the pre-edit data-loss
//   hazard this task builds the foundation for). Child elements are written as empty tags — the
//   §4.24 example sends <Supported><contacts:JobTitle/><contacts:OfficeLocation/></Supported>.
// - §2.2.3.29.2 Collection (Sync) strict child order: SyncKey, CollectionId, Supported,
//   DeletesAsMoves, GetChanges, WindowSize, ConversationMode, Options, Commands — Supported sits
//   BETWEEN CollectionId and DeletesAsMoves.
// - Tokens: Supported = page 0, 0x20 (AIRSYNC_TOKENS); JobTitle = page 1, 0x28 and OfficeLocation =
//   page 1, 0x2C (CONTACTS_TOKENS), both verified in code_pages.rs.

/// Contacts code page index (code_pages.rs page 1).
const PAGE_CONTACTS: u8 = 1;
/// Contacts `JobTitle` token (page 1, 0x28 — CONTACTS_TOKENS).
const CONTACTS_JOB_TITLE: u8 = 0x28;
/// Contacts `OfficeLocation` token (page 1, 0x2C — CONTACTS_TOKENS).
const CONTACTS_OFFICE_LOCATION: u8 = 0x2C;

/// The [MS-ASCMD] §4.24 request shape: an initial Contacts Sync whose
/// Supported list names JobTitle + OfficeLocation.
fn supported_sync_request() -> SyncRequest {
    SyncRequest {
        collection_id: "2".to_string(),
        sync_key: "0".to_string(),
        class: "Contacts".to_string(),
        window_size: 5,
        filter_age_days: 0,
        fetch_body: false,
        truncation_size: None,
        mime_support: None,
        mime_truncation: None,
        supported: Some(vec![
            SupportedElement {
                page: PAGE_CONTACTS,
                token: CONTACTS_JOB_TITLE,
            },
            SupportedElement {
                page: PAGE_CONTACTS,
                token: CONTACTS_OFFICE_LOCATION,
            },
        ]),
    }
}

/// With `supported` set, `<Supported>` must sit immediately after
/// CollectionId and immediately before DeletesAsMoves — the strict
/// Collection child order of [MS-ASCMD] §2.2.3.29.2 — and carry the
/// listed element tags as empty children in order (§4.24 shape).
#[test]
fn build_sync_request_emits_supported_between_collection_id_and_deletes_as_moves() {
    let collection = sync_collection_for(&supported_sync_request(), "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x20), // Supported (page 0, AIRSYNC_TOKENS)
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "Supported must sit between CollectionId and DeletesAsMoves (§2.2.3.29.2 order)"
    );

    let supported = &collection.children[2];
    assert_eq!(supported.tag_name(), "Supported");
    assert_eq!(
        supported
            .children
            .iter()
            .map(|c| (c.page, c.token))
            .collect::<Vec<_>>(),
        vec![
            (PAGE_CONTACTS, CONTACTS_JOB_TITLE),
            (PAGE_CONTACTS, CONTACTS_OFFICE_LOCATION),
        ],
        "Supported children are the listed element tags, in the listed order"
    );
    assert_eq!(supported.children[0].tag_name(), "JobTitle");
    assert_eq!(supported.children[1].tag_name(), "OfficeLocation");
    for child in &supported.children {
        assert_eq!(
            child.value,
            WbxmlValue::Empty,
            "Supported children are empty tags (§4.24 example shape)"
        );
    }
}

/// `supported: None` must keep the request byte-identical to the
/// pre-Supported shape: no Supported token anywhere, children exactly
/// [SyncKey, CollectionId, DeletesAsMoves, GetChanges, WindowSize].
#[test]
fn build_sync_request_omits_supported_when_none() {
    let mut req = supported_sync_request();
    req.supported = None;
    let collection = sync_collection_for(&req, "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "supported=None must be byte-identical to the pre-Supported shape"
    );
}

/// `supported: Some([])` is treated as absent — the builder must NOT
/// emit an empty `<Supported/>` (that wire form means "ghost everything"
/// per §2.2.3.179 rule 3, which no caller wants; an empty list reads as
/// rule 1: nothing ghosted, element omitted).
#[test]
fn build_sync_request_omits_supported_when_empty_vec() {
    let mut req = supported_sync_request();
    req.supported = Some(Vec::new());
    let collection = sync_collection_for(&req, "14.0");

    assert_eq!(
        collection_child_tokens(&collection),
        vec![
            (PAGE_AIRSYNC, AS_SYNC_KEY),
            (PAGE_AIRSYNC, AS_COLLECTION_ID),
            (PAGE_AIRSYNC, 0x1E), // DeletesAsMoves (page 0)
            (PAGE_AIRSYNC, AS_GET_CHANGES),
            (PAGE_AIRSYNC, AS_WINDOW_SIZE),
        ],
        "supported=Some([]) must omit Supported entirely (same shape as None)"
    );
}

/// A Sync request carrying Supported must survive the WBXML codec
/// losslessly — the page-1 children inside the page-0 Supported
/// container exercise the SWITCH_PAGE path in both directions.
#[test]
fn sync_request_with_supported_round_trips() {
    let tree = build_sync_request(&supported_sync_request(), "14.0");
    let back = round_trip(&tree);
    assert_eq!(
        tree, back,
        "Supported and its page-1 children must survive encode/decode"
    );
}
