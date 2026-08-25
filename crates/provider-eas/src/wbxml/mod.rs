// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// ActiveSync WBXML codec. Implements the subset of WBXML defined in
// [MS-ASWBXML] — no string tables, no entities, no attributes, UTF-8 only.
// Consists of:
//
//   * `serializer` / `Serializer` — streaming encoder + tree-based shortcut
//   * `deserializer` / `Deserializer` — streaming decoder + tree-based shortcut
//   * `code_pages` — 26 MS-ASWBXML code pages (tag tables)
//   * `types` — `WbxmlElement` / `WbxmlValue` tree types
//   * `tags` — tag id constants for common pages
//   * `error` / `WbxmlError` — error enum used across the codec

/// The 26 MS-ASWBXML code pages as `(token, tag_name)` tables.
pub mod code_pages;
/// Streaming WBXML byte decoder plus the tree-based shortcut.
pub mod deserializer;
/// `WbxmlError` / `WbxmlResult` for the whole codec.
pub mod error;
/// WBXML global tokens (switch page, end, etc.).
pub mod global_tokens;
/// Streaming WBXML byte encoder plus the tree-based shortcut.
pub mod serializer;
/// Tag id constants for the common code pages.
pub mod tags;
/// The `WbxmlElement` / `WbxmlValue` tree types both directions share.
pub mod types;

// Convenience re-exports at the module root so callers can write
// `provider_eas::wbxml::serialize_tree(...)` without spelunking
// through the submodule tree.
pub use code_pages::{CodePage, code_page, is_valid_page, is_valid_tag};
pub use deserializer::{Deserializer, DeserializerEvent, deserialize_to_tree};
pub use error::{WbxmlError, WbxmlResult};
pub use serializer::{Serializer, serialize_tree};
pub use types::{WbxmlElement, WbxmlValue};

#[cfg(test)]
mod round_trip_tests {
    //! Round-trip tests covering all 26 MS-ASWBXML code pages. Each test
    //! builds a representative `WbxmlElement` tree with tags from that page,
    //! serializes to bytes, deserializes back, and asserts equality.

    use code_pages::code_page;

    use super::*;

    /// Pull the first registered `(token, name)` pair from a code page, for
    /// tests that need a known-good tag.
    fn sample_token(page: u8) -> (u8, &'static str) {
        let cp = code_page(page).expect("page exists");
        *cp.tokens.first().expect("page has at least one token")
    }

    /// Generic per-page round trip: one empty tag, one text tag, one opaque
    /// tag, nested two-deep.
    fn round_trip_page(page: u8) {
        let (tok, name) = sample_token(page);
        let _ = name; // unused, but useful in debug

        let root = WbxmlElement::container(
            page,
            tok,
            vec![
                WbxmlElement::empty(page, tok),
                WbxmlElement::text(page, tok, format!("page={page} tag={name}")),
                WbxmlElement::opaque(page, tok, vec![page, tok, 0xAA, 0xBB]),
                WbxmlElement::container(page, tok, vec![WbxmlElement::text(page, tok, "nested")]),
            ],
        );

        let bytes = serialize_tree(&root).expect("serialize");
        let back = deserialize_to_tree(&bytes).expect("deserialize");
        assert_eq!(root, back, "page {page} round trip mismatch");
    }

    #[test]
    fn round_trip_page_0_airsync() {
        round_trip_page(0);
    }

    #[test]
    fn round_trip_page_1_contacts() {
        round_trip_page(1);
    }

    #[test]
    fn round_trip_page_2_email() {
        round_trip_page(2);
    }

    #[test]
    fn round_trip_page_4_calendar() {
        round_trip_page(4);
    }

    #[test]
    fn round_trip_page_5_move() {
        round_trip_page(5);
    }

    #[test]
    fn round_trip_page_6_getitemestimate() {
        round_trip_page(6);
    }

    #[test]
    fn round_trip_page_7_folderhierarchy() {
        round_trip_page(7);
    }

    #[test]
    fn round_trip_page_8_meetingresponse() {
        round_trip_page(8);
    }

    #[test]
    fn round_trip_page_9_tasks() {
        round_trip_page(9);
    }

    #[test]
    fn round_trip_page_10_resolverecipients() {
        round_trip_page(10);
    }

    #[test]
    fn round_trip_page_11_validatecert() {
        round_trip_page(11);
    }

    #[test]
    fn round_trip_page_12_contacts2() {
        round_trip_page(12);
    }

    #[test]
    fn round_trip_page_13_ping() {
        round_trip_page(13);
    }

    #[test]
    fn round_trip_page_14_provision() {
        round_trip_page(14);
    }

    #[test]
    fn round_trip_page_15_search() {
        round_trip_page(15);
    }

    #[test]
    fn round_trip_page_16_gal() {
        round_trip_page(16);
    }

    #[test]
    fn round_trip_page_17_airsyncbase() {
        round_trip_page(17);
    }

    #[test]
    fn round_trip_page_18_settings() {
        round_trip_page(18);
    }

    #[test]
    fn round_trip_page_19_documentlibrary() {
        round_trip_page(19);
    }

    #[test]
    fn round_trip_page_20_itemoperations() {
        round_trip_page(20);
    }

    #[test]
    fn round_trip_page_21_composemail() {
        round_trip_page(21);
    }

    #[test]
    fn round_trip_page_22_email2() {
        round_trip_page(22);
    }

    #[test]
    fn round_trip_page_23_notes() {
        round_trip_page(23);
    }

    #[test]
    fn round_trip_page_24_rightsmanagement() {
        round_trip_page(24);
    }

    #[test]
    fn round_trip_page_25_find() {
        round_trip_page(25);
    }

    /// Page 3 (AirNotify) is deprecated and empty. We can't build a tag from
    /// it, so just assert the code page exists and is empty.
    #[test]
    fn page_3_airnotify_is_empty() {
        let cp = code_page(3).expect("page 3 exists");
        assert!(cp.tokens.is_empty());
    }

    /// Realistic FolderSync request shape.
    #[test]
    fn round_trip_foldersync_request() {
        use tags::folder;
        let tree = WbxmlElement::container(
            7,
            folder::FOLDER_SYNC,
            vec![WbxmlElement::text(7, folder::SYNC_KEY, "0")],
        );
        let bytes = serialize_tree(&tree).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(tree, back);
    }

    /// Realistic Sync request with multiple collections.
    #[test]
    fn round_trip_sync_request() {
        use tags::airsync;
        let tree = WbxmlElement::container(
            0,
            airsync::SYNC,
            vec![
                WbxmlElement::container(
                    0,
                    airsync::COLLECTIONS,
                    vec![WbxmlElement::container(
                        0,
                        airsync::COLLECTION,
                        vec![
                            WbxmlElement::text(0, airsync::SYNC_KEY, "{abcdef}"),
                            WbxmlElement::text(0, airsync::COLLECTION_ID, "1"),
                            WbxmlElement::empty(0, airsync::DELETE),
                        ],
                    )],
                ),
                WbxmlElement::text(0, airsync::WINDOW_SIZE, "10"),
            ],
        );
        let bytes = serialize_tree(&tree).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(tree, back);
    }

    /// Deep nesting (10 levels).
    #[test]
    fn round_trip_deep_nesting() {
        let (tok, _) = sample_token(0);
        let mut leaf = WbxmlElement::text(0, tok, "deep");
        for _ in 0..10 {
            leaf = WbxmlElement::container(0, tok, vec![leaf]);
        }
        let bytes = serialize_tree(&leaf).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(leaf, back);
    }

    /// Opaque data with binary bytes (incl. high bit set) round-trips.
    #[test]
    fn round_trip_binary_opaque() {
        let (tok, _) = sample_token(0);
        let data: Vec<u8> = (0..=255).collect();
        let tree = WbxmlElement::opaque(0, tok, data);
        let bytes = serialize_tree(&tree).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(tree, back);
    }

    /// UTF-8 text with multi-byte characters round-trips.
    #[test]
    fn round_trip_utf8_text() {
        let (tok, _) = sample_token(0);
        let tree = WbxmlElement::text(0, tok, "héllo 世界 🌐");
        let bytes = serialize_tree(&tree).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(tree, back);
    }

    /// Long opaque length (must encode as multibyte uint).
    #[test]
    fn round_trip_large_opaque() {
        let (tok, _) = sample_token(0);
        let data = vec![0x42u8; 500];
        let tree = WbxmlElement::opaque(0, tok, data);
        let bytes = serialize_tree(&tree).unwrap();
        let back = deserialize_to_tree(&bytes).unwrap();
        assert_eq!(tree, back);
    }
}
