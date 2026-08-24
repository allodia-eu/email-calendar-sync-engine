//! Unit tests for the body-warming work list: which messages still owe a fetch.
//!
//! Split from `tests.rs`, which covers the list reads themselves, to keep both inside the
//! 500-line limit.

use super::{
    tests::{account, keys, open, seed},
    *,
};

#[test]
fn the_body_warming_list_holds_messages_missing_either_half_of_their_content() {
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "warm",
        Some("2026-01-02T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "cold",
        Some("2026-01-01T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    // Warm means **both** halves cached: the extracted text and the raw source it came from.
    conn.execute(
        "INSERT INTO message_body (account, provider_key, plain, fetched_at)
         VALUES ('a', 'warm', 'text', '2026-01-02T00:00:00Z')",
        [],
    )
    .expect("body");
    conn.execute(
        "INSERT INTO message_source (account, provider_key, content_hash, fetched_at, size_octets)
         VALUES ('a', 'warm', 'hash-warm', '2026-01-02T00:00:00Z', 10)",
        [],
    )
    .expect("source");

    // The same key on another account holds a body; the cache is keyed by account, so it says
    // nothing about this one's.
    seed(
        &conn,
        "scope-b",
        "b",
        "warm",
        Some("2026-01-03T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );

    // A message whose source a lowered size cap dropped keeps its text — and must still come
    // back as work, or raising the cap again would fetch nothing.
    seed(
        &conn,
        "scope-a",
        "a",
        "text-only",
        Some("2026-01-04T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    conn.execute(
        "INSERT INTO message_body (account, provider_key, plain, fetched_at)
         VALUES ('a', 'text-only', 'text', '2026-01-04T00:00:00Z')",
        [],
    )
    .expect("body");

    let rows = mail_missing_body(&conn, &[account("a")], usize::MAX).expect("missing");
    assert_eq!(keys(&rows), vec!["text-only", "cold"]);
    let both =
        mail_missing_body(&conn, &[account("a"), account("b")], usize::MAX).expect("missing");
    assert_eq!(keys(&both), vec!["text-only", "warm", "cold"]);
    assert!(
        mail_missing_body(&conn, &[], usize::MAX)
            .expect("missing")
            .is_empty()
    );
}
