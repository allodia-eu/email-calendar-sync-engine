//! Unit tests for [`purge_domain`]: it drops one domain of one account, scopes, derived rows,
//! caches and queued writes, and leaves the account's other domains and every other account as
//! they were.

use engine_core::{
    ids::{AccountId, AddressBookId, DavCollectionId, MailboxId},
    sync::{JmapDataType, SearchDomain, SyncScope},
    write::PendingOpKind,
};
use rusqlite::Connection;

use super::{CONTACT_TABLES, MAIL_TABLES, purge_domain};
use crate::convert::{kind_to_text, scope_key};

fn id(value: &str) -> AccountId {
    AccountId::try_from(value).unwrap()
}

/// One scope per domain for `account`, plus a JMAP type this build does not know.
fn scopes(account: &str) -> [(SyncScope, &'static str); 4] {
    [
        (
            SyncScope::ImapMailbox {
                account: id(account),
                mailbox: MailboxId::try_from("INBOX").unwrap(),
            },
            "mail",
        ),
        (
            SyncScope::DavCollection {
                account: id(account),
                collection: DavCollectionId::try_from("/dav/cal/default/").unwrap(),
            },
            "calendar",
        ),
        (
            SyncScope::CardDavAddressBook {
                account: id(account),
                address_book: AddressBookId::try_from("personal").unwrap(),
            },
            "contacts",
        ),
        (
            SyncScope::JmapType {
                account: id(account),
                data_type: JmapDataType::Other("Quota".to_owned()),
            },
            "unknown",
        ),
    ]
}

fn exec(conn: &Connection, sql: &str, params: impl rusqlite::Params) {
    conn.execute(sql, params).unwrap();
}

/// Seeds accounts `a` and `b` alike: every scope carries a cursor, an object and its
/// full-text row, and the account-keyed caches and queued writes of all three domains.
fn seed() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn).unwrap();
    for account in ["a", "b"] {
        for (scope, tag) in scopes(account) {
            let key = scope_key(&scope);
            exec(
                &conn,
                "INSERT INTO sync_scope (scope_key, account, token, cursor) VALUES (?1, ?2, 1, 'c')",
                (&key, account),
            );
            exec(
                &conn,
                "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, ?2, '{}')",
                (&key, tag),
            );
            exec(
                &conn,
                "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
                 VALUES (?1, ?2, ?2, '', '')",
                (&key, tag),
            );
        }
        for table in MAIL_TABLES.iter().chain(CONTACT_TABLES) {
            seed_account_row(&conn, table, account);
        }
        for (email, suppressed) in [("kept@example.test", 0), ("forgotten@example.test", 1)] {
            exec(
                &conn,
                "INSERT INTO recipient_observation (account, source_message, email, suppressed)
                 VALUES (?1, 'm1', ?2, ?3)",
                (account, email, suppressed),
            );
        }
        for kind in PendingOpKind::ALL {
            exec(
                &conn,
                "INSERT INTO pending_op
                     (account, idempotency_key, resource_key, depends_on, payload, state, token, kind)
                 VALUES (?1, ?2, 'res', '[]', '{}', 'Queued', 1, ?2)",
                (account, kind_to_text(kind)),
            );
        }
        // A row from before the store recorded kinds: nobody can say whose it is.
        exec(
            &conn,
            "INSERT INTO pending_op
                 (account, idempotency_key, resource_key, depends_on, payload, state, token)
             VALUES (?1, 'legacy', 'res', '[]', '{}', 'Queued', 1)",
            [account],
        );
    }
    conn
}

fn seed_account_row(conn: &Connection, table: &str, account: &str) {
    let sql = match table {
        "message_source" => {
            "INSERT INTO message_source (account, provider_key, content_hash, fetched_at)
             VALUES (?1, 'k', 'hash', '2026-01-01T00:00:00Z')"
        }
        "message_body" => {
            "INSERT INTO message_body (account, provider_key, plain, fetched_at)
             VALUES (?1, 'k', 'text', '2026-01-01T00:00:00Z')"
        }
        "recipient_coverage" => {
            "INSERT INTO recipient_coverage (account, window_json, sent_collection_present)
             VALUES (?1, '{}', 1)"
        }
        "recipient_index_state" => {
            "INSERT INTO recipient_index_state (account, version) VALUES (?1, 1)"
        }
        "server_keywords" => {
            "INSERT INTO server_keywords (account, provider_key, keywords) VALUES (?1, 'k', '[]')"
        }
        "contact_photo" => {
            "INSERT INTO contact_photo
                 (account, contact, resource, fingerprint, content_hash, fetched_at)
             VALUES (?1, 'c', 'r', 'f', 'hash', '2026-01-01T00:00:00Z')"
        }
        other => panic!("no seed for {other}"),
    };
    exec(conn, sql, [account]);
}

fn count(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> i64 {
    conn.query_row(sql, params, |r| r.get(0)).unwrap()
}

/// How many rows a scope still owns across its cursor row, objects and full-text rows.
fn scope_rows(conn: &Connection, account: &str, tag: &str) -> i64 {
    let (scope, _) = scopes(account)
        .into_iter()
        .find(|(_, t)| *t == tag)
        .unwrap();
    let key = scope_key(&scope);
    ["sync_scope", "object", "fts_doc"]
        .iter()
        .map(|table| {
            count(
                conn,
                &format!("SELECT count(*) FROM {table} WHERE scope_key = ?1"),
                [&key],
            )
        })
        .sum()
}

fn ops(conn: &Connection, account: &str, domain: SearchDomain) -> i64 {
    PendingOpKind::ALL
        .into_iter()
        .filter(|kind| kind.domain() == domain)
        .map(|kind| {
            count(
                conn,
                "SELECT count(*) FROM pending_op WHERE account = ?1 AND kind = ?2",
                (account, kind_to_text(kind)),
            )
        })
        .sum()
}

/// How many rows `seed` gives each account across `tables`: one per table.
fn seeded(tables: &[&str]) -> i64 {
    i64::try_from(tables.len()).unwrap()
}

fn account_rows(conn: &Connection, tables: &[&str], account: &str) -> i64 {
    tables
        .iter()
        .map(|table| {
            count(
                conn,
                &format!("SELECT count(*) FROM {table} WHERE account = ?1"),
                [account],
            )
        })
        .sum()
}

fn generation(conn: &Connection) -> i64 {
    count(conn, "SELECT generation FROM contact_state", [])
}

/// Everything account `b` was seeded with is still there.
fn assert_b_untouched(conn: &Connection) {
    for tag in ["mail", "calendar", "contacts", "unknown"] {
        assert_eq!(scope_rows(conn, "b", tag), 3, "b lost its {tag} scope");
    }
    assert_eq!(account_rows(conn, MAIL_TABLES, "b"), seeded(MAIL_TABLES));
    assert_eq!(account_rows(conn, CONTACT_TABLES, "b"), 1);
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM pending_op WHERE account = 'b'",
            []
        ),
        15
    );
}

#[test]
fn forgetting_the_calendar_leaves_mail_and_contacts_as_they_were() {
    let mut conn = seed();
    let before = generation(&conn);
    purge_domain(&mut conn, "a", SearchDomain::Calendar).unwrap();

    assert_eq!(scope_rows(&conn, "a", "calendar"), 0);
    assert_eq!(ops(&conn, "a", SearchDomain::Calendar), 0);
    assert_eq!(scope_rows(&conn, "a", "mail"), 3);
    assert_eq!(scope_rows(&conn, "a", "contacts"), 3);
    assert_eq!(ops(&conn, "a", SearchDomain::Mail), 6);
    assert_eq!(ops(&conn, "a", SearchDomain::Contacts), 3);
    assert_eq!(account_rows(&conn, MAIL_TABLES, "a"), seeded(MAIL_TABLES));
    assert_eq!(account_rows(&conn, CONTACT_TABLES, "a"), 1);
    assert_eq!(generation(&conn), before, "no contact changed");
    assert_b_untouched(&conn);
}

#[test]
fn forgetting_mail_drops_its_caches_and_recipient_history_but_keeps_what_was_forgotten() {
    let mut conn = seed();
    purge_domain(&mut conn, "a", SearchDomain::Mail).unwrap();

    assert_eq!(scope_rows(&conn, "a", "mail"), 0);
    assert_eq!(ops(&conn, "a", SearchDomain::Mail), 0);
    assert_eq!(account_rows(&conn, MAIL_TABLES, "a"), 0);
    // The observation the user asked to forget stays suppressed, so the same Sent mail
    // synced again cannot bring the address back; the rest of the history goes.
    let observations: Vec<String> = conn
        .prepare("SELECT email FROM recipient_observation WHERE account = 'a'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(observations, ["forgotten@example.test"]);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM message_body_fts", []),
        1,
        "only b's body is still searchable"
    );

    assert_eq!(scope_rows(&conn, "a", "calendar"), 3);
    assert_eq!(scope_rows(&conn, "a", "contacts"), 3);
    assert_eq!(account_rows(&conn, CONTACT_TABLES, "a"), 1);
    assert_b_untouched(&conn);
}

#[test]
fn forgetting_contacts_drops_photos_and_moves_the_contact_generation() {
    let mut conn = seed();
    let before = generation(&conn);
    purge_domain(&mut conn, "a", SearchDomain::Contacts).unwrap();

    assert_eq!(scope_rows(&conn, "a", "contacts"), 0);
    assert_eq!(ops(&conn, "a", SearchDomain::Contacts), 0);
    assert_eq!(account_rows(&conn, CONTACT_TABLES, "a"), 0);
    assert_eq!(
        generation(&conn),
        before + 1,
        "the people index is now stale"
    );

    assert_eq!(scope_rows(&conn, "a", "mail"), 3);
    assert_eq!(scope_rows(&conn, "a", "calendar"), 3);
    assert_eq!(account_rows(&conn, MAIL_TABLES, "a"), seeded(MAIL_TABLES));
    assert_b_untouched(&conn);
}

/// A scope or a queued write the build cannot place in a domain is kept by every domain
/// forget: guessing would drop data the host never asked to lose. Forgetting the account is
/// what removes them.
#[test]
fn what_cannot_be_placed_in_a_domain_is_kept() {
    let mut conn = seed();
    for domain in [
        SearchDomain::Mail,
        SearchDomain::Calendar,
        SearchDomain::Contacts,
    ] {
        purge_domain(&mut conn, "a", domain).unwrap();
    }
    assert_eq!(scope_rows(&conn, "a", "unknown"), 3);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM pending_op WHERE account = 'a' AND kind IS NULL",
            []
        ),
        1
    );
}

/// Every account-keyed table is either named for one domain here, or handled by a rule of
/// its own (queued writes by their kind, recipient history by its suppression). A new table
/// that is neither would outlive a domain forget unnoticed.
#[test]
fn every_account_keyed_table_belongs_to_a_domain() {
    for table in super::super::ACCOUNT_TABLES {
        let placed = MAIL_TABLES.contains(table)
            || CONTACT_TABLES.contains(table)
            || ["pending_op", "recipient_observation"].contains(table);
        assert!(placed, "{table} belongs to no domain");
    }
}
