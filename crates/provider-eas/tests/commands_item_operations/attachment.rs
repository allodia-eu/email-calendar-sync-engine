// SPDX-License-Identifier: MPL-2.0
//! ItemOperations attachment fetch: round-trip and attachment-data response parse.

use super::*;

#[test]
fn item_operations_request_attachment_round_trips() {
    let req = ItemOperationsFetchRequest {
        server_id: "srv-1".to_string(),
        collection_id: "col-1".to_string(),
        file_reference: Some("fileref-abc".to_string()),
        long_id: None,
        mime: false,
        accept_multipart: false,
    };
    let tree = build_item_operations_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

#[test]
fn item_operations_response_parses_attachment_data() {
    use tags::item_operations as io;
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        io::ITEM_OPERATIONS,
        vec![WbxmlElement::container(
            PAGE_ITEM_OPS,
            io::RESPONSE,
            vec![WbxmlElement::container(
                PAGE_ITEM_OPS,
                io::FETCH,
                vec![
                    WbxmlElement::text(PAGE_ITEM_OPS, io::STATUS, "1"),
                    WbxmlElement::container(
                        PAGE_ITEM_OPS,
                        io::PROPERTIES,
                        vec![
                            WbxmlElement::text(PAGE_ITEM_OPS, io::DATA, "QkFTRTY0REFUQQ=="),
                            WbxmlElement::text(pages::BASE, tags::base::CONTENT_TYPE, "image/png"),
                        ],
                    ),
                ],
            )],
        )],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.content_type.as_deref(), Some("image/png"));
    assert_eq!(parsed.data.as_deref(), Some("QkFTRTY0REFUQQ=="));
}
