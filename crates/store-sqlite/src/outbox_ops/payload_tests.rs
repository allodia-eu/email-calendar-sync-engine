//! Where an op's payload lives, and for how long: beside its row while something may still
//! dispatch it, and nowhere once nothing can. The row itself stays, because it is the
//! idempotency record.

use core::time::Duration;
use std::path::Path;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, ProviderKey},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_store::{
    ClaimRejection, Confirmation, LeaseRequest, LeasedPendingOp, ManualClock, PendingOpClaim,
    Store, StoreRead, WorkerId,
};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::SqliteStore;

fn account() -> AccountId {
    AccountId::try_from("acct-payload").unwrap()
}

fn op(kind: PendingOpKind, name: &str, payload: Value) -> PendingOp {
    PendingOp::new(
        IdempotencyKey::new(name).unwrap(),
        kind,
        ResourceKey::new(format!("res:{name}")).unwrap(),
        payload,
    )
}

/// A file store, so a second raw connection can read what it wrote.
fn file_store(dir: &tempfile::TempDir) -> SqliteStore<ManualClock> {
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap());
    SqliteStore::open(dir.path().join("store.db"), clock).unwrap()
}

fn raw(dir: &tempfile::TempDir) -> Connection {
    Connection::open(dir.path().join("store.db")).unwrap()
}

/// How many ops have a payload stored.
fn stored_payloads(dir: &tempfile::TempDir) -> i64 {
    raw(dir)
        .query_row("SELECT count(*) FROM pending_op_payload", [], |r| r.get(0))
        .unwrap()
}

async fn claim(store: &SqliteStore<ManualClock>, id: PendingOpId) -> LeasedPendingOp {
    let request = LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5));
    match store
        .claim_pending_op(account(), id, request)
        .await
        .unwrap()
    {
        PendingOpClaim::Leased(leased) => *leased,
        PendingOpClaim::Refused(why) => panic!("{id:?} refused: {why:?}"),
    }
}

fn succeeded() -> PendingOutcome {
    PendingOutcome::Succeeded {
        provider_key: ProviderKey::new("stored").unwrap(),
    }
}

fn failed(class: FailureClass) -> PendingOutcome {
    PendingOutcome::Failed {
        class,
        retry_after: None,
    }
}

#[tokio::test]
async fn a_settled_op_keeps_its_key_and_drops_its_payload() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    let save = op(
        PendingOpKind::MailDraftPut,
        "save",
        json!({ "draft": "text" }),
    );
    let id = store
        .enqueue_pending_op(account(), save.clone())
        .await
        .unwrap();
    let leased = claim(&store, id).await;
    assert_eq!(leased.op.payload, json!({ "draft": "text" }));

    store
        .mark_pending_op(&leased.lease, succeeded())
        .await
        .unwrap();

    assert_eq!(stored_payloads(&dir), 0);
    // The row is the idempotency record: the same save again is the same, settled op.
    assert_eq!(store.enqueue_pending_op(account(), save).await.unwrap(), id);
    let request = LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5));
    assert!(matches!(
        store
            .claim_pending_op(account(), id, request)
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    ));
}

#[tokio::test]
async fn a_failed_send_keeps_its_payload_and_sends_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    let send = op(
        PendingOpKind::MailSubmit,
        "send",
        json!({ "to": "a@example.com" }),
    );
    let id = store.enqueue_pending_op(account(), send).await.unwrap();
    let leased = claim(&store, id).await;
    store
        .mark_pending_op(&leased.lease, failed(FailureClass::Permanent))
        .await
        .unwrap();

    assert_eq!(stored_payloads(&dir), 1);
    let listed = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(listed[0].payload, json!({ "to": "a@example.com" }));
    assert_eq!(
        store.retry_pending_op_now(account(), id).await.unwrap(),
        None
    );
    assert_eq!(
        claim(&store, id).await.op.payload,
        json!({ "to": "a@example.com" })
    );
}

#[tokio::test]
async fn a_send_awaiting_confirmation_keeps_its_payload_until_it_is_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    let mut ids = Vec::new();
    for name in ["not-delivered", "delivered"] {
        let id = store
            .enqueue_pending_op(account(), op(PendingOpKind::MailSubmit, name, json!(name)))
            .await
            .unwrap();
        let leased = claim(&store, id).await;
        let unknown = PendingOutcome::NeedsConfirmation {
            detail: "the connection closed after the message was handed over".to_owned(),
        };
        store.mark_pending_op(&leased.lease, unknown).await.unwrap();
        ids.push(id);
    }
    assert_eq!(stored_payloads(&dir), 2);

    for (id, answer) in ids
        .iter()
        .zip([Confirmation::NotDelivered, Confirmation::Delivered])
    {
        assert_eq!(
            store
                .confirm_pending_op(account(), *id, answer)
                .await
                .unwrap(),
            None
        );
    }

    assert_eq!(stored_payloads(&dir), 1);
    assert_eq!(
        claim(&store, ids[0]).await.op.payload,
        json!("not-delivered")
    );
}

#[tokio::test]
async fn a_withdrawn_op_and_a_failed_edit_drop_their_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    let withdrawn = store
        .enqueue_pending_op(account(), op(PendingOpKind::MailDraftPut, "w", json!(1)))
        .await
        .unwrap();
    let edit = store
        .enqueue_pending_op(account(), op(PendingOpKind::MailEdit, "e", json!(2)))
        .await
        .unwrap();
    let retried = store
        .enqueue_pending_op(account(), op(PendingOpKind::MailEdit, "r", json!(3)))
        .await
        .unwrap();

    assert_eq!(
        store.cancel_pending_op(account(), withdrawn).await.unwrap(),
        None
    );
    let leased = claim(&store, edit).await;
    store
        .mark_pending_op(&leased.lease, failed(FailureClass::Conflict))
        .await
        .unwrap();
    // A retryable failure parks the op rather than settling it: it is sent again later.
    let leased = claim(&store, retried).await;
    store
        .mark_pending_op(&leased.lease, failed(FailureClass::Retryable))
        .await
        .unwrap();

    let kept: Vec<i64> = raw(&dir)
        .prepare("SELECT op_id FROM pending_op_payload")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kept, vec![crate::convert::op_id_to_i64(retried).unwrap()]);
}

/// The number of frames in the WAL: every page a commit wrote since the last checkpoint.
fn wal_frames(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))
        .unwrap()
}

/// Claiming and recording an outcome change a few columns, and must write only those: a
/// draft's payload carries its attachments, and is written once, when it is queued.
#[tokio::test]
async fn claiming_and_parking_an_op_does_not_write_its_payload_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    let attachment = "x".repeat(2_000_000);
    let id = store
        .enqueue_pending_op(
            account(),
            op(
                PendingOpKind::MailDraftPut,
                "large",
                json!({ "attachment": attachment }),
            ),
        )
        .await
        .unwrap();
    let observer = raw(&dir);
    // A busy checkpoint leaves the enqueue's frames in the WAL, so it must not pass silently.
    let busy: i64 = observer
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy, 0, "the WAL was not emptied before the claim");

    let leased = claim(&store, id).await;
    store
        .mark_pending_op(&leased.lease, failed(FailureClass::Retryable))
        .await
        .unwrap();

    // 2 MB is about 500 pages of 4 KiB; rewriting the row would write each of them again.
    let frames = wal_frames(&observer);
    assert!(frames < 40, "a claim and a mark wrote {frames} pages");
    assert_eq!(stored_payloads(&dir), 1);
}

/// The trigger's `WHEN` restates `engine_store::stays_listed` in SQL. Pinned for every kind and
/// state, so the two cannot drift: a payload dropped from an op the queue read still lists
/// makes that read fail for the whole account.
#[test]
fn the_trigger_keeps_a_payload_exactly_while_the_op_stays_listed() {
    use engine_store::{PendingOpState, stays_listed};

    let mut conn = Connection::open_in_memory().unwrap();
    crate::pool::tune(&conn, false).unwrap();
    crate::migrations::migrate(&mut conn).unwrap();
    let kinds = PendingOpKind::ALL.map(Some).into_iter().chain([None]);
    for kind in kinds {
        for state in [
            PendingOpState::Pending,
            PendingOpState::InFlight,
            PendingOpState::NeedsConfirmation,
            PendingOpState::Succeeded,
            PendingOpState::Failed,
            PendingOpState::Cancelled,
        ] {
            let text = kind.map(crate::convert::kind_to_text);
            conn.execute(
                "INSERT INTO pending_op
                     (account, kind, idempotency_key, resource_key, depends_on, state, token)
                 VALUES ('acct', ?1, ?2, 'res', '[]', 'Pending', 0)",
                (text, format!("{text:?}-{state:?}")),
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO pending_op_payload (op_id, payload) VALUES (?1, '{}')",
                [id],
            )
            .unwrap();
            conn.execute(
                "UPDATE pending_op SET state = ?1 WHERE id = ?2",
                (crate::convert::state_to_text(state), id),
            )
            .unwrap();
            let kept: bool = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM pending_op_payload WHERE op_id = ?1)",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(kept, stays_listed(kind, state), "{kind:?} in {state:?}");
        }
    }
}

#[tokio::test]
async fn forgetting_an_account_takes_its_payloads_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir);
    store
        .enqueue_pending_op(account(), op(PendingOpKind::MailSubmit, "s", json!(1)))
        .await
        .unwrap();

    store.forget_account(&account()).await.unwrap();

    assert_eq!(stored_payloads(&dir), 0);
}

/// One `pending_op` row as a build before the payload moved wrote it.
fn insert_old_op(conn: &Connection, key: &str, kind: &str, state: &str) {
    conn.execute(
        "INSERT INTO pending_op
             (account, kind, idempotency_key, resource_key, depends_on, payload, state, token,
              attempts)
         VALUES ('acct-payload', ?1, ?2, ?2, '[]', json_object('key', ?2), ?3, 1, 0)",
        (kind, key, state),
    )
    .unwrap();
}

fn migrate_old_store(path: &Path) {
    let mut conn = Connection::open(path).unwrap();
    crate::migrations::migrate_to(&mut conn, 19).unwrap();
    for (key, kind, state) in [
        ("queued", "MailDraftPut", "Pending"),
        ("unsure", "MailSubmit", "NeedsConfirmation"),
        ("unsent", "MailSubmit", "Failed"),
        ("saved", "MailDraftPut", "Succeeded"),
        ("refused", "MailEdit", "Failed"),
        ("withdrawn", "MailSubmit", "Cancelled"),
    ] {
        insert_old_op(&conn, key, kind, state);
    }
}

/// An upgraded store keeps the payload of every op that may still be dispatched, and drops
/// the rest; each row stays, so a settled op still answers for its key.
#[tokio::test]
async fn the_upgrade_keeps_only_the_payloads_something_may_still_send() {
    let dir = tempfile::tempdir().unwrap();
    migrate_old_store(&dir.path().join("store.db"));

    let store = file_store(&dir);
    assert_eq!(store.schema_status().await.unwrap().migrated_from, Some(19));

    let conn = raw(&dir);
    let kept: Vec<String> = conn
        .prepare(
            "SELECT o.idempotency_key FROM pending_op o
               JOIN pending_op_payload p ON p.op_id = o.id ORDER BY o.id",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kept, ["queued", "unsure", "unsent"]);
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM pending_op", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 6);
    let columns: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('pending_op') WHERE name = 'payload'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0);

    let listed = store.list_pending_ops(account()).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(
        listed
            .iter()
            .all(|row| row.payload == json!({ "key": row.idempotency_key.as_str() }))
    );
}
