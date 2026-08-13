//! Unit tests for the mail list read: the plans it must take, and what it returns.

use rusqlite::Connection;

use super::*;

fn account(value: &str) -> AccountId {
    AccountId::try_from(value).expect("valid account")
}

/// A migrated database with two accounts' scopes registered.
fn open() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open");
    crate::migrations::migrate(&mut conn).expect("schema");
    for (scope, acct) in [("scope-a", "a"), ("scope-b", "b")] {
        conn.execute(
            "INSERT INTO sync_scope (scope_key, account, token) VALUES (?1, ?2, 1)",
            (scope, acct),
        )
        .expect("scope");
    }
    conn
}

/// Seeds one message straight into the projected tables, which is what the apply path leaves.
#[expect(clippy::too_many_arguments, reason = "one row's columns")]
fn seed(
    conn: &Connection,
    scope: &str,
    acct: &str,
    key: &str,
    date: Option<&str>,
    thread: Option<&str>,
    flags: i64,
    mailboxes: &[&str],
) {
    conn.execute(
        "INSERT INTO message (scope_key, provider_key, account, thread_id, message_id, date_utc,
                              flags, has_attachment, from_name, from_addr, subject, preview)
         VALUES (?1, ?2, ?3, ?4, 'mid@example.com', ?5, ?6, 1, 'Alice', 'alice@example.com',
                 'Subject', 'Preview')",
        rusqlite::params![scope, key, acct, thread, date, flags],
    )
    .expect("message");
    for mailbox in mailboxes {
        conn.execute(
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, 'mailbox', ?3)",
            (scope, key, *mailbox),
        )
        .expect("membership");
    }
}

/// The `detail` column of every step of a query's plan, joined.
fn plan(conn: &Connection, sql: &str, params: &[Value]) -> String {
    crate::sql::query_all(
        conn,
        &format!("EXPLAIN QUERY PLAN {sql}"),
        rusqlite::params_from_iter(params.iter()),
        |row| row.get::<_, String>(3),
    )
    .expect("explain")
    .join(" | ")
}

fn keys(rows: &[MailListRow]) -> Vec<String> {
    rows.iter()
        .map(|row| row.mail.key.as_str().to_owned())
        .collect()
}

/// An index whose query does not plan through it is write cost for no read benefit, and no other
/// test in the suite can tell the difference — the read returns the same rows either way. What
/// separates "the first page costs the page" from "the first page costs the mailbox" is precisely
/// the absence of a sort over every row, so that is what is asserted here.
#[test]
fn the_windowed_read_is_ordered_by_an_index_not_a_sort() {
    let conn = open();
    for (accounts, expected_index) in [
        (vec![account("a")], "message_account_date"),
        (vec![account("a"), account("b")], "message_date"),
    ] {
        let sql = format!(
            "SELECT {COLUMNS} FROM message m {} WHERE m.account IN ({}) {ORDER} LIMIT ?{}",
            ordering_index(accounts.len()),
            placeholders(accounts.len()),
            accounts.len() + 1
        );
        let mut params: Vec<Value> = accounts
            .iter()
            .map(|a| Value::Text(a.as_str().to_owned()))
            .collect();
        params.push(Value::Integer(100));
        let plan = plan(&conn, &sql, &params);
        assert!(
            !plan.contains("TEMP B-TREE FOR ORDER BY"),
            "the window would be cut after sorting every row: {plan}"
        );
        assert!(
            plan.contains(expected_index),
            "expected the read to walk {expected_index}: {plan}"
        );
    }
}

/// Expanding a conversation and resolving a named message are seeks, not scans.
#[test]
fn the_targeted_reads_seek_their_indices() {
    let conn = open();
    for (column, expected_index) in [
        ("m.thread_id", "message_account_thread"),
        ("m.provider_key", "message_account_key"),
    ] {
        let sql = format!("SELECT {COLUMNS} FROM message m WHERE m.account = ?1 AND {column} = ?2");
        let plan = plan(
            &conn,
            &sql,
            &[Value::Text("a".into()), Value::Text("x".into())],
        );
        assert!(
            plan.contains(expected_index),
            "expected the read to seek {expected_index}: {plan}"
        );
        assert!(
            !plan.contains("SCAN message"),
            "a scan of the message table means the index is unused: {plan}"
        );
    }
}

#[test]
fn the_window_is_newest_first_with_undated_mail_last() {
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "m-old",
        Some("2026-01-01T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "m-new",
        Some("2026-01-03T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(&conn, "scope-a", "a", "m-none", None, None, 0, &["inbox"]);

    let rows = list_mail(&conn, &[account("a")], &Selector::Newest, usize::MAX).expect("list");
    assert_eq!(keys(&rows), vec!["m-new", "m-old", "m-none"]);
    let page = list_mail(&conn, &[account("a")], &Selector::Newest, 2).expect("list");
    assert_eq!(keys(&page), vec!["m-new", "m-old"]);
}

#[test]
fn a_row_carries_what_a_list_renders_without_opening_a_payload() {
    let conn = open();
    // `$seen` | `$flagged`, the bits the projection writes.
    seed(
        &conn,
        "scope-a",
        "a",
        "m1",
        Some("2026-01-01T00:00:00Z"),
        Some("t1"),
        0b11,
        &["inbox", "archive"],
    );
    let rows = list_mail(&conn, &[account("a")], &Selector::Newest, usize::MAX).expect("list");
    let row = &rows[0];
    assert_eq!(row.account, account("a"));
    assert_eq!(row.mail.subject.as_deref(), Some("Subject"));
    assert_eq!(row.mail.from_name.as_deref(), Some("Alice"));
    assert_eq!(row.mail.from_addr.as_deref(), Some("alice@example.com"));
    assert_eq!(row.mail.preview.as_deref(), Some("Preview"));
    assert_eq!(
        row.mail.thread_id.as_ref().map(ThreadId::as_str),
        Some("t1")
    );
    assert!(row.mail.has_attachment);
    assert!(row.mail.flags.seen() && row.mail.flags.flagged());
    assert!(!row.mail.flags.is_unread());
    let mut mailboxes: Vec<&str> = row.mailboxes.iter().map(MailboxId::as_str).collect();
    mailboxes.sort_unstable();
    assert_eq!(mailboxes, vec!["archive", "inbox"]);
}

#[test]
fn a_message_filed_nowhere_still_lists() {
    // Membership is a separate axis, and the list read joins it optionally: a message whose
    // junction rows have not landed yet must not vanish from the mailbox.
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "m1",
        Some("2026-01-01T00:00:00Z"),
        None,
        0,
        &[],
    );
    let rows = list_mail(&conn, &[account("a")], &Selector::Newest, usize::MAX).expect("list");
    assert_eq!(keys(&rows), vec!["m1"]);
    assert!(rows[0].mailboxes.is_empty());
}

#[test]
fn accounts_merge_into_one_date_order_and_an_unnamed_account_contributes_nothing() {
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "a1",
        Some("2026-01-02T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-b",
        "b",
        "b1",
        Some("2026-01-03T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-b",
        "b",
        "b2",
        Some("2026-01-01T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );

    let rows = list_mail(
        &conn,
        &[account("a"), account("b")],
        &Selector::Newest,
        usize::MAX,
    )
    .expect("list");
    assert_eq!(keys(&rows), vec!["b1", "a1", "b2"]);
    let only_a = list_mail(&conn, &[account("a")], &Selector::Newest, usize::MAX).expect("list");
    assert_eq!(keys(&only_a), vec!["a1"]);
    assert!(
        list_mail(&conn, &[], &Selector::Newest, usize::MAX)
            .expect("list")
            .is_empty()
    );
}

#[test]
fn a_conversation_reads_back_whole_and_in_order() {
    let conn = open();
    let thread = ThreadId::try_from("t1").unwrap();
    seed(
        &conn,
        "scope-a",
        "a",
        "m1",
        Some("2026-01-01T00:00:00Z"),
        Some("t1"),
        0,
        &["inbox"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "m2",
        Some("2026-01-05T00:00:00Z"),
        Some("t1"),
        0,
        &["sent"],
    );
    seed(
        &conn,
        "scope-a",
        "a",
        "m3",
        Some("2026-01-04T00:00:00Z"),
        Some("t2"),
        0,
        &["inbox"],
    );

    let rows = list_mail(
        &conn,
        &[account("a")],
        &Selector::Threads(vec![thread]),
        usize::MAX,
    )
    .expect("list");
    assert_eq!(
        keys(&rows),
        vec!["m2", "m1"],
        "newest first, other threads left out"
    );
}

#[test]
fn named_keys_resolve_outside_any_window() {
    let conn = open();
    seed(
        &conn,
        "scope-a",
        "a",
        "m1",
        Some("2026-01-01T00:00:00Z"),
        None,
        0,
        &["inbox"],
    );
    let rows = list_mail(
        &conn,
        &[account("a")],
        &Selector::Keys(vec![
            ProviderKey::new("m1").unwrap(),
            ProviderKey::new("gone").unwrap(),
        ]),
        usize::MAX,
    )
    .expect("list");
    assert_eq!(keys(&rows), vec!["m1"]);
}

#[test]
fn the_body_warming_list_holds_only_messages_with_no_cached_text() {
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
    conn.execute(
        "INSERT INTO message_body (account, provider_key, plain, fetched_at)
         VALUES ('a', 'warm', 'text', '2026-01-02T00:00:00Z')",
        [],
    )
    .expect("body");

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

    let rows = mail_missing_body(&conn, &[account("a")], usize::MAX).expect("missing");
    assert_eq!(keys(&rows), vec!["cold"]);
    let both =
        mail_missing_body(&conn, &[account("a"), account("b")], usize::MAX).expect("missing");
    assert_eq!(keys(&both), vec!["warm", "cold"]);
    assert!(
        mail_missing_body(&conn, &[], usize::MAX)
            .expect("missing")
            .is_empty()
    );
}

#[test]
fn an_empty_selector_names_nothing_and_never_reaches_the_store() {
    assert!(own(MailSelector::Threads(&[])).is_none());
    assert!(own(MailSelector::Keys(&[])).is_none());
    assert!(own(MailSelector::Newest).is_some());
}
