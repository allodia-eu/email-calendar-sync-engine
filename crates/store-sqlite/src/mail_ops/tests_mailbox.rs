//! The mailbox reads: the plans they take and the rows they return.

use super::{
    mailbox::{in_mailbox, oldest_in_mailbox, oldest_sql, span_sql},
    tests::{account, keys, open, plan, seed},
    *,
};

fn at(text: &str) -> UtcDateTime {
    text.parse().expect("instant")
}

/// Both reads walk the account's date index and probe the membership per row. A plan that
/// sorted instead would read every message in the account to answer a question about one
/// mailbox, and return the same rows while it did.
#[test]
fn the_mailbox_reads_walk_the_date_index_without_a_sort() {
    let conn = open();
    let span = plan(
        &conn,
        &span_sql(1),
        &[
            Value::Text("a".into()),
            Value::Text("2024-01-01T00:00:00Z".into()),
            Value::Text("2026-01-01T00:00:00Z".into()),
            Value::Text("sent".into()),
            Value::Integer(100),
        ],
    );
    let oldest = plan(
        &conn,
        &oldest_sql(),
        &[Value::Text("a".into()), Value::Text("sent".into())],
    );
    for plan in [span, oldest] {
        assert!(plan.contains("message_account_date"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
}

#[test]
fn a_span_reads_one_accounts_mailbox_and_the_oldest_ignores_the_span() {
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "s1",
        Some("2025-06-01T09:00:00Z"),
        None,
        0,
        &["sent"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "s2",
        Some("2024-06-01T09:00:00Z"),
        None,
        0,
        &["sent"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "s3",
        Some("2023-06-01T09:00:00Z"),
        None,
        0,
        &["sent"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "i1",
        Some("2025-01-01T09:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-b",
        "b",
        "b1",
        Some("2025-01-01T09:00:00Z"),
        None,
        0,
        &["sent"],
    );
    let sent = MailboxId::try_from("sent").unwrap();

    let rows = in_mailbox(
        &conn,
        &[account("a")],
        &sent,
        (at("2024-01-01T00:00:00Z"), at("2026-01-01T00:00:00Z")),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(keys(&rows), vec!["s1", "s2"]);
    assert_eq!(
        oldest_in_mailbox(&conn, &account("a"), &sent).unwrap(),
        Some(at("2023-06-01T09:00:00Z"))
    );
}
