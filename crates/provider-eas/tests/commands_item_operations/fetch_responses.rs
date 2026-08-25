// SPDX-License-Identifier: MPL-2.0
//! ItemOperations fetch responses: spec shape, body payload, type-4 fallback, status.

use super::*;

/// Spec-shaped response parse: ItemOperations(20,0x05) > Status(20,0x0D)"1"
/// + Response(20,0x0E) > Fetch(20,0x06) > { Status(20,0x0D)"1", Properties(20,0x0B) > {
///   Data(20,0x0C), airsyncbase:ContentType(17,0x17) } }. Regression guard: the old constants read
///   Response as 0x08 (Options), Status as 0x0A (Total), Properties as 0x0F (Version), Data as 0x10
///   (Schema), ContentType as page-20 0x12 (EmptyFolderContents) — so every live response parsed to
///   status 0 / no data.
#[test]
fn item_operations_response_parses_spec_shaped_response() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05, // ItemOperations
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level Status
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E, // Response
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06, // Fetch
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // fetch Status
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B, // Properties
                            vec![
                                WbxmlElement::text(PAGE_ITEM_OPS, 0x0C, "aGVsbG8="), // Data
                                WbxmlElement::text(pages::BASE, 0x17, "text/plain"), /* airsyncbase:ContentType */
                            ],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some("aGVsbG8="));
    assert_eq!(parsed.content_type.as_deref(), Some("text/plain"));
}

/// An item/body fetch returns the payload as Properties >
/// airsyncbase:Body(17,0x0A) > { Type(17,0x06), Data(17,0x0B) } rather
/// than a page-20 Data element. The parser must surface Body.Data as
/// `result.data` and derive content_type from the Body Type when the
/// server omits airsyncbase:ContentType. Live evidence 2026-08-02:
/// Exchange 2019 answers exactly this shape.
#[test]
fn item_operations_response_parses_body_fetch_payload() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "2"), // Type=HTML
                                    WbxmlElement::text(pages::BASE, 0x0B, "<p>hi</p>"), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some("<p>hi</p>"));
    assert_eq!(parsed.content_type.as_deref(), Some("text/html"));
}

/// Task 3: a MIME fetch answer carries Properties > airsyncbase:Body with
/// Type=4 (MIME BLOB, [MS-ASCMD] §4.10.2.2 response example). The parser
/// must surface Body.Data as `result.data` and, when the server omits
/// airsyncbase:ContentType, fall back to `message/rfc822` — mirroring the
/// Type 1/2 → text/plain|html fallbacks.
#[test]
fn item_operations_response_type_4_body_falls_back_to_message_rfc822() {
    let raw_mime = "From: a@b\r\nSubject: opaque s + e\r\n\r\nbody";
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![
                        WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"),
                        WbxmlElement::container(
                            PAGE_ITEM_OPS,
                            0x0B,
                            vec![WbxmlElement::container(
                                pages::BASE,
                                0x0A, // airsyncbase:Body
                                vec![
                                    WbxmlElement::text(pages::BASE, 0x06, "4"), // Type = MIME
                                    WbxmlElement::text(pages::BASE, 0x0C, "13813"), /* EstimatedDataSize */
                                    WbxmlElement::text(pages::BASE, 0x0B, raw_mime), // Data
                                ],
                            )],
                        ),
                    ],
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.data.as_deref(), Some(raw_mime));
    assert_eq!(
        parsed.content_type.as_deref(),
        Some("message/rfc822"),
        "Type-4 body without an explicit ContentType must read as message/rfc822"
    );
}

/// A fetch-level failure status (e.g. 3 = not found) must override the
/// top-level success Status, matching the Sync parser's "more specific
/// wins" rule.
#[test]
fn item_operations_response_fetch_status_overrides_top_level() {
    let response = WbxmlElement::container(
        PAGE_ITEM_OPS,
        0x05,
        vec![
            WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "1"), // top-level OK
            WbxmlElement::container(
                PAGE_ITEM_OPS,
                0x0E,
                vec![WbxmlElement::container(
                    PAGE_ITEM_OPS,
                    0x06,
                    vec![WbxmlElement::text(PAGE_ITEM_OPS, 0x0D, "3")], // fetch failed
                )],
            ),
        ],
    );
    let parsed = parse_item_operations_response(&response).expect("parse");
    assert_eq!(parsed.status, 3);
    assert!(parsed.data.is_none());
}
