// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

fn el_text(el: &WbxmlElement) -> &str {
    match &el.value {
        WbxmlValue::Text(t) => t,
        other => panic!("expected text value, got {:?}", other),
    }
}

#[test]
fn search_mailbox_request_wire_shape() {
    let req = SearchRequest {
        store: "Mailbox".to_string(),
        query: "Presentation".to_string(),
        collection_id: Some("7".to_string()),
        range: "0-4".to_string(),
        deep_traversal: true,
    };
    let tree = build_search_request(&req);
    assert_eq!((tree.page, tree.token), (15, 0x05), "Search root");

    let store = &tree.children[0];
    assert_eq!((store.page, store.token), (15, 0x07), "Store");
    assert_eq!(
        (store.children[0].page, store.children[0].token),
        (15, 0x08),
        "Name"
    );
    assert_eq!(el_text(&store.children[0]), "Mailbox");

    let query = &store.children[1];
    assert_eq!((query.page, query.token), (15, 0x09), "Query");
    let and = &query.children[0];
    assert_eq!((and.page, and.token), (15, 0x13), "And");
    assert_eq!(
        (and.children[0].page, and.children[0].token),
        (0, 0x10),
        "airsync:Class"
    );
    assert_eq!(el_text(&and.children[0]), "Email");
    assert_eq!(
        (and.children[1].page, and.children[1].token),
        (0, 0x12),
        "airsync:CollectionId"
    );
    assert_eq!(el_text(&and.children[1]), "7");
    assert_eq!(
        (and.children[2].page, and.children[2].token),
        (15, 0x15),
        "FreeText"
    );
    assert_eq!(el_text(&and.children[2]), "Presentation");

    let options = &store.children[2];
    assert_eq!((options.page, options.token), (15, 0x0A), "Options");
    assert_eq!(
        (options.children[0].page, options.children[0].token),
        (15, 0x19),
        "RebuildResults"
    );
    assert_eq!(
        (options.children[1].page, options.children[1].token),
        (15, 0x0B),
        "Range"
    );
    assert_eq!(el_text(&options.children[1]), "0-4");
    assert_eq!(
        (options.children[2].page, options.children[2].token),
        (15, 0x17),
        "DeepTraversal"
    );
}

#[test]
fn search_mailbox_request_omits_unset_options() {
    let req = SearchRequest {
        store: "Mailbox".to_string(),
        query: "hello".to_string(),
        collection_id: None,
        range: "0-49".to_string(),
        deep_traversal: false,
    };
    let tree = build_search_request(&req);
    let store = &tree.children[0];
    let and = &store.children[1].children[0];
    assert_eq!(
        and.children.len(),
        2,
        "only Class + FreeText, no CollectionId"
    );
    assert_eq!(
        (and.children[1].page, and.children[1].token),
        (15, 0x15),
        "FreeText"
    );
    let options = &store.children[2];
    assert_eq!(
        options.children.len(),
        2,
        "RebuildResults + Range, no DeepTraversal"
    );
    assert_eq!(
        (options.children[1].page, options.children[1].token),
        (15, 0x0B),
        "Range"
    );
}

#[test]
fn search_gal_request_query_is_plain_text() {
    let req = SearchRequest {
        store: "GAL".to_string(),
        query: "Anat".to_string(),
        collection_id: None,
        range: "0-1".to_string(),
        deep_traversal: true,
    };
    let tree = build_search_request(&req);
    let store = &tree.children[0];
    assert_eq!(el_text(&store.children[0]), "GAL");
    let query = &store.children[1];
    assert_eq!((query.page, query.token), (15, 0x09), "Query");
    assert_eq!(el_text(query), "Anat");
    assert!(
        query.children.is_empty(),
        "GAL query is a leaf, not an And container"
    );
    let options = &store.children[2];
    assert_eq!(
        (options.children[0].page, options.children[0].token),
        (15, 0x0B),
        "Range"
    );
    assert_eq!(
        (options.children[1].page, options.children[1].token),
        (15, 0x19),
        "RebuildResults"
    );
    assert_eq!(
        (options.children[2].page, options.children[2].token),
        (15, 0x17),
        "DeepTraversal"
    );
}

#[test]
fn search_request_round_trips() {
    let req = SearchRequest {
        store: "Mailbox".to_string(),
        query: "Sales Totals".to_string(),
        collection_id: Some("7".to_string()),
        range: "0-4".to_string(),
        deep_traversal: true,
    };
    let tree = build_search_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn search_mailbox_response_parses_results() {
    let properties = WbxmlElement::container(
        15,
        0x0F, // Properties
        vec![
            WbxmlElement::text(2, 0x14, "Sales Totals for April"), // email:Subject
            WbxmlElement::text(2, 0x18, "\"deviceuser2\" <chris@contoso.com>"), // email:From
            WbxmlElement::text(2, 0x15, "1"),                      // email:Read
        ],
    );
    let result = WbxmlElement::container(
        15,
        0x0E, // Result
        vec![
            WbxmlElement::text(0, 0x10, "Email"),     // airsync:Class
            WbxmlElement::text(15, 0x18, "RgAAAA=="), // LongId
            WbxmlElement::text(0, 0x12, "7"),         // airsync:CollectionId
            properties,
        ],
    );
    let store = WbxmlElement::container(
        15,
        0x07, // Store
        vec![
            WbxmlElement::text(15, 0x0C, "1"), // Status
            result,
            WbxmlElement::text(15, 0x0B, "0-0"), // Range
            WbxmlElement::text(15, 0x10, "1"),   // Total
        ],
    );
    let root = WbxmlElement::container(
        15,
        0x05, // Search
        vec![
            WbxmlElement::text(15, 0x0C, "1"),              // Status
            WbxmlElement::container(15, 0x0D, vec![store]), // Response
        ],
    );

    let parsed = parse_search_response(&root).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.store_status, Some(1));
    assert_eq!(parsed.range.as_deref(), Some("0-0"));
    assert_eq!(parsed.total, Some(1));
    assert_eq!(parsed.results.len(), 1);
    let r = &parsed.results[0];
    assert_eq!(r.class.as_deref(), Some("Email"));
    assert_eq!(r.long_id.as_deref(), Some("RgAAAA=="));
    assert_eq!(r.collection_id.as_deref(), Some("7"));
    let item = r.item.as_ref().expect("mailbox result carries an EasItem");
    assert_eq!(item.subject.as_deref(), Some("Sales Totals for April"));
    assert_eq!(
        item.from.as_deref(),
        Some("\"deviceuser2\" <chris@contoso.com>")
    );
    assert_eq!(item.read, Some(true));
    assert!(r.gal.is_none());
}

#[test]
fn search_gal_response_parses_entries() {
    let properties = WbxmlElement::container(
        15,
        0x0F, // Properties
        vec![
            WbxmlElement::text(16, 0x05, "Anat Kerry"), // gal:DisplayName
            WbxmlElement::text(16, 0x06, "+1 (301) 5550156 X8376"), // gal:Phone
            WbxmlElement::text(16, 0x07, "Bldg36/6163"), // gal:Office
            WbxmlElement::text(16, 0x08, "SDE"),        // gal:Title
            WbxmlElement::text(16, 0x09, "Contoso"),    // gal:Company
            WbxmlElement::text(16, 0x0A, "anatk"),      // gal:Alias
            WbxmlElement::text(16, 0x0B, "Anat"),       // gal:FirstName
            WbxmlElement::text(16, 0x0C, "Kerry"),      // gal:LastName
            WbxmlElement::text(16, 0x0E, "+1 (953) 5550167"), // gal:MobilePhone
            WbxmlElement::text(16, 0x0F, "anatk@contoso.com"), // gal:EmailAddress
        ],
    );
    let result = WbxmlElement::container(15, 0x0E, vec![properties]);
    let store = WbxmlElement::container(
        15,
        0x07,
        vec![
            WbxmlElement::text(15, 0x0C, "1"),
            result,
            WbxmlElement::text(15, 0x0B, "0-1"),
            WbxmlElement::text(15, 0x10, "11"),
        ],
    );
    let root = WbxmlElement::container(
        15,
        0x05,
        vec![
            WbxmlElement::text(15, 0x0C, "1"),
            WbxmlElement::container(15, 0x0D, vec![store]),
        ],
    );

    let parsed = parse_search_response(&root).expect("parse");
    assert_eq!(parsed.total, Some(11));
    assert_eq!(parsed.results.len(), 1);
    let gal = parsed.results[0]
        .gal
        .as_ref()
        .expect("GAL result carries a GalEntry");
    assert_eq!(gal.display_name.as_deref(), Some("Anat Kerry"));
    assert_eq!(gal.phone.as_deref(), Some("+1 (301) 5550156 X8376"));
    assert_eq!(gal.office.as_deref(), Some("Bldg36/6163"));
    assert_eq!(gal.title.as_deref(), Some("SDE"));
    assert_eq!(gal.company.as_deref(), Some("Contoso"));
    assert_eq!(gal.alias.as_deref(), Some("anatk"));
    assert_eq!(gal.first_name.as_deref(), Some("Anat"));
    assert_eq!(gal.last_name.as_deref(), Some("Kerry"));
    assert_eq!(gal.mobile_phone.as_deref(), Some("+1 (953) 5550167"));
    assert_eq!(gal.email_address.as_deref(), Some("anatk@contoso.com"));
    assert!(parsed.results[0].item.is_none());
}

#[test]
fn search_response_error_status_yields_empty_results() {
    let root = WbxmlElement::container(15, 0x05, vec![WbxmlElement::text(15, 0x0C, "2")]);
    let parsed = parse_search_response(&root).expect("parse");
    assert_eq!(parsed.status, 2);
    assert!(parsed.results.is_empty());
    assert!(parsed.store_status.is_none());
}
