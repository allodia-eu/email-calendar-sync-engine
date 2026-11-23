//! Routing and packing, against the limits Stalwart states.

use super::*;
use crate::session_limits::parse_limits;

/// Stalwart's core limits (`tests/fixtures/session_shared_accounts.json`).
fn stalwart() -> CoreLimits {
    parse_limits(&serde_json::json!({
        "maxSizeUpload": 50_000_000, "maxConcurrentUpload": 4, "maxSizeRequest": 10_000_000,
        "maxConcurrentRequests": 4, "maxCallsInRequest": 16, "maxObjectsInGet": 500,
        "maxObjectsInSet": 500
    }))
}

const BLOBS: BlobUpload = BlobUpload::Supported {
    max_size: Some(7_499_488),
};

#[test]
fn base64_takes_four_bytes_for_every_three_begun() {
    assert_eq!(base64_len(0), 0);
    assert_eq!(base64_len(1), 4);
    assert_eq!(base64_len(3), 4);
    assert_eq!(base64_len(4), 8);
    assert_eq!(base64_len(7_499_488), 9_999_320);
}

#[test]
fn a_small_message_goes_inline_and_one_too_large_for_a_blob_by_url() {
    let budget = Budget::new(&stalwart(), BLOBS, 1);
    assert_eq!(budget.item(20_000, 100).route, Route::Inline);
    // Larger than `maxSizeBlobSet`.
    assert_eq!(budget.item(7_499_489, 100).route, Route::ByUrl);
}

/// Stalwart states `maxSizeBlobSet` (7,499,488) so that the largest blob's base64 (9,999,320)
/// still fits a 10 MB request: the two limits are one limit.
#[test]
fn the_largest_blob_stalwart_takes_still_fits_its_request() {
    let budget = Budget::new(&stalwart(), BLOBS, 1);
    assert_eq!(budget.item(7_499_488, 100).route, Route::Inline);
}

#[test]
fn a_blob_whose_base64_does_not_fit_a_request_goes_by_url() {
    let small_requests = CoreLimits {
        max_size_request: 5_000_000,
        ..stalwart()
    };
    let budget = Budget::new(&small_requests, BLOBS, 1);
    assert_eq!(budget.item(4_000_000, 100).route, Route::ByUrl);
    assert_eq!(budget.item(3_000_000, 100).route, Route::Inline);
}

#[test]
fn without_the_blob_extension_or_room_for_two_calls_everything_goes_by_url() {
    assert_eq!(
        Budget::new(&stalwart(), BlobUpload::Unsupported, 1)
            .item(10, 10)
            .route,
        Route::ByUrl
    );
    let one_call = CoreLimits {
        max_calls_in_request: 1,
        ..stalwart()
    };
    assert_eq!(
        Budget::new(&one_call, BLOBS, 1).item(10, 10).route,
        Route::ByUrl
    );
    let unlimited = BlobUpload::Supported { max_size: None };
    assert_eq!(
        Budget::new(&stalwart(), unlimited, 1).item(10, 10).route,
        Route::Inline
    );
}

#[test]
fn requests_close_at_the_byte_budget() {
    let budget = Budget::new(&stalwart(), BLOBS, 1);
    // Three messages whose base64 is 4 MB each: two fit 10 MB, the third does not.
    let item = budget.item(3_000_000, 200);
    assert_eq!(budget.pack(&[item; 3]), vec![vec![0, 1], vec![2]]);
}

#[test]
fn requests_close_at_the_object_budget() {
    let few = CoreLimits {
        max_objects_in_set: 2,
        ..stalwart()
    };
    let budget = Budget::new(&few, BLOBS, 1);
    let item = budget.item(10, 100);
    assert_eq!(
        budget.pack(&[item; 5]),
        vec![vec![0, 1], vec![2, 3], vec![4]]
    );
}

#[test]
fn a_message_by_url_costs_only_its_entry() {
    let budget = Budget::new(&stalwart(), BLOBS, 1);
    let large = budget.item(9_000_000, 200);
    let small = budget.item(3_000_000, 200);
    assert_eq!(large.route, Route::ByUrl);
    assert_eq!(
        budget.pack(&[large, small, large, small]),
        vec![vec![0, 1, 2, 3]]
    );
    assert_eq!(budget.pack(&[]), Vec::<Vec<usize>>::new());
}
