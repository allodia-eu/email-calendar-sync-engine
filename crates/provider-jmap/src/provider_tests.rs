//! Provider read/sync tests: mailbox/email/calendar snapshots and deltas,
//! snapshot pagination, raw message-source download, and error propagation —
//! driven offline by the shared `provider_test_support` harness.

use engine_core::sync::{SyncUpdate, SyncWindow};
use engine_provider::{EmailChunk, EmailStream};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{provider_test_support::*, *};

/// Drains an email chunk stream into its chunks, so a test can assert the pass's
/// aggregate shape (the intra-pass paging is now the adapter's internal detail).
async fn drain(mut stream: EmailStream<'_>) -> Vec<EmailChunk> {
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    chunks
}

/// The number of messages upserted across a drained pass's chunks.
fn upserted(chunks: &[EmailChunk]) -> usize {
    chunks.iter().map(|c| c.changed.len()).sum()
}

#[tokio::test]
async fn message_source_downloads_the_blob_and_substitutes_the_template() {
    const MIME: &[u8] = b"From: a@example.com\r\nSubject: Hi\r\n\r\nBody text\r\n";
    let exec = FakeExecutor::new(vec![]).with_download_body(MIME);
    let raw = crate::blob::message_source(&exec, &message_with_blob("m1", "blob-1"))
        .await
        .unwrap();
    assert_eq!(raw.as_bytes(), MIME);
    // The download template's origin was rebased to the connection and every
    // placeholder substituted (mail account `c`, the message's blob id). Substitutions
    // are percent-encoded — RFC 6570 level-1 simple expansion, which is what RFC 8620
    // §6.2 specifies — so the media type's `/` arrives as `%2F`.
    assert_eq!(
        exec.download_urls.lock().unwrap().as_slice(),
        ["http://127.0.0.1:18080/download/c/blob-1/message?accept=application%2Foctet-stream"]
    );
}

#[tokio::test]
async fn fetch_message_source_provider_method_returns_the_raw_mime() {
    let exec = FakeExecutor::new(vec![]).with_download_body(b"raw-bytes");
    let provider = JmapProvider::with_executor(Box::new(exec));
    // Advertises the capability now that a download template is present.
    assert!(provider.connection_info().capabilities.message_source());
    let raw = provider
        .fetch_message_source(&account(), &message_with_blob("m1", "b1"))
        .await
        .unwrap();
    assert_eq!(raw.as_bytes(), b"raw-bytes");
}

#[tokio::test]
async fn message_source_without_a_blob_id_is_a_protocol_error() {
    use engine_core::{
        ids::{MailboxId, MessageId},
        membership::Memberships,
    };
    let exec = FakeExecutor::new(vec![]).with_download_body(b"x");
    let message = engine_core::mail::Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    let err = crate::blob::message_source(&exec, &message)
        .await
        .unwrap_err();
    assert!(matches!(err, JmapError::Protocol(_)));
}

#[tokio::test]
async fn mailbox_first_sync_snapshots_all_collections() {
    let p = provider(vec![fixture("mailbox_snapshot_response.json")]);
    let sync = p.sync_mailboxes(&account(), None).await.unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 7);
    assert_eq!(present.len(), 7);
    assert!(!sync.next_cursor.as_str().is_empty());
}

#[tokio::test]
async fn email_first_sync_is_a_complete_snapshot() {
    let p = provider(vec![fixture("email_snapshot_response.json")]);
    let sync = p.sync_email(&account(), None).await.unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &sync.update else {
        panic!("expected snapshot");
    };
    // All 9 seed emails, complete present set (total within one page).
    assert_eq!(objects.len(), 9);
    assert_eq!(present.len(), 9);
}

/// A synthetic `Email/query`+`Email/get` page response (one minimal email per id),
/// so snapshot paging can be driven offline without a multi-page fixture.
fn email_query_page(ids: &[&str], total: usize) -> Value {
    let list: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "mailboxIds": { "a": true } }))
        .collect();
    json!({
        "methodResponses": [
            ["Email/query", { "accountId": "c", "queryState": "q", "position": 0, "ids": ids, "total": total }, "0"],
            ["Email/get", { "accountId": "c", "state": "sX", "list": list, "notFound": [] }, "1"]
        ]
    })
}

#[tokio::test]
async fn email_snapshot_pages_chain_until_exhausted() {
    // Three emails over two internal pages of two (fetch_batch = 2): the stream drains
    // both and yields a reconciling pass whose present set covers all three.
    let p = provider(vec![
        email_query_page(&["e1", "e2"], 3),
        email_query_page(&["e3"], 3),
    ]);

    let chunks = drain(p.stream_email(&account(), None, SyncWindow::full(), 2, 0)).await;
    assert_eq!(upserted(&chunks), 3);
    let present: usize = chunks.iter().map(|c| c.present.len()).sum();
    assert_eq!(present, 3, "every snapshot id rides the reconcile chunks");
    // The pass total shows from the first commit; the last chunk tombstones+advances.
    assert_eq!(chunks[0].total, Some(3));
    assert!(chunks.last().unwrap().is_reconcile_final());
}

#[tokio::test]
async fn email_snapshot_without_total_pages_until_a_short_page() {
    // A server that omits `total` (no `calculateTotal` support): paging must keep
    // going while pages come back full and stop on the first short page.
    let page = |ids: &[&str]| {
        let list: Vec<Value> = ids
            .iter()
            .map(|id| json!({ "id": id, "mailboxIds": { "a": true } }))
            .collect();
        json!({
            "methodResponses": [
                ["Email/query", { "accountId": "c", "queryState": "q", "position": 0, "ids": ids }, "0"],
                ["Email/get", { "accountId": "c", "state": "sX", "list": list, "notFound": [] }, "1"]
            ]
        })
    };
    let p = provider(vec![page(&["e1", "e2"]), page(&["e3"])]);

    let chunks = drain(p.stream_email(&account(), None, SyncWindow::full(), 2, 0)).await;
    assert_eq!(chunks[0].total, None, "no total advertised");
    assert_eq!(upserted(&chunks), 3, "both pages drained despite no total");
}

#[tokio::test]
async fn email_delta_pages_follow_has_more_changes() {
    // Page one reports more changes and resolves a created id; page two finishes
    // and reports a destroy. The continuation resumes from page one's newState.
    let page1 = json!({
        "methodResponses": [
            ["Email/changes", { "newState": "s2", "hasMoreChanges": true, "created": ["e1"], "updated": [], "destroyed": [] }, "0"],
            ["Email/get", { "state": "s2", "list": [{ "id": "e1", "mailboxIds": { "a": true } }], "notFound": [] }, "1"],
            ["Email/get", { "state": "s2", "list": [], "notFound": [] }, "2"]
        ]
    });
    let page2 = json!({
        "methodResponses": [
            ["Email/changes", { "newState": "s3", "hasMoreChanges": false, "created": [], "updated": [], "destroyed": ["e0"] }, "0"],
            ["Email/get", { "state": "s3", "list": [], "notFound": [] }, "1"],
            ["Email/get", { "state": "s3", "list": [], "notFound": [] }, "2"]
        ]
    });
    let p = provider(vec![page1, page2]);

    let chunks = drain(p.stream_email(
        &account(),
        Some(&SyncState::new("s1")),
        SyncWindow::full(),
        1,
        0,
    ))
    .await;
    // An additive delta: one created upsert, one destroyed key, advancing to s3.
    assert_eq!(upserted(&chunks), 1);
    let removed: usize = chunks.iter().map(|c| c.removed.len()).sum();
    assert_eq!(removed, 1);
    let last = chunks.last().unwrap();
    assert!(!last.is_reconcile_final(), "a delta never tombstones");
    assert_eq!(last.advance_to.as_ref().unwrap().as_str(), "s3");
}

#[tokio::test]
async fn email_delta_with_cursor_uses_changes_then_get() {
    let p = provider(vec![fixture("email_changes_response.json")]);
    let sync = p
        .sync_email(&account(), Some(&SyncState::new("sb2")))
        .await
        .unwrap();
    // An empty delta still exercises the changes→get back-reference path.
    assert!(!sync.is_snapshot());
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &sync.update
    else {
        panic!("expected delta");
    };
    assert!(changed.is_empty());
    assert!(removed.is_empty());
    assert_eq!(sync.next_cursor.as_str(), "sb2");
}

#[tokio::test]
async fn cannot_calculate_changes_falls_back_to_snapshot() {
    // First response: Email/changes errors; second: a full snapshot.
    let error_changes = json!({
        "methodResponses": [["error", { "type": "cannotCalculateChanges" }, "0"]]
    });
    let p = provider(vec![error_changes, fixture("email_snapshot_response.json")]);
    let sync = p
        .sync_email(&account(), Some(&SyncState::new("stale")))
        .await
        .unwrap();
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected snapshot after resync");
    };
    assert_eq!(objects.len(), 9);
}

#[tokio::test]
async fn calendar_first_sync_snapshots_collections_and_events() {
    let calendars = provider(vec![fixture("calendar_snapshot_response.json")])
        .sync_calendars(&account(), None)
        .await
        .unwrap();
    assert!(calendars.is_snapshot());
    let SyncUpdate::Snapshot { objects, present } = &calendars.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 1);
    assert_eq!(present.len(), 1);

    let events = provider(vec![fixture("event_snapshot_response.json")])
        .sync_events(&account(), None)
        .await
        .unwrap();
    assert!(events.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 6);
    // JSCalendar recurrence survived the full fetch→normalize path.
    assert!(
        objects
            .iter()
            .any(engine_core::calendar::Event::is_recurring)
    );
}

#[tokio::test]
async fn mailbox_delta_with_cursor_uses_changes_then_get() {
    let response = json!({
        "methodResponses": [
            ["Mailbox/changes", { "newState": "s2", "created": ["x"], "updated": [], "destroyed": ["y"] }, "0"],
            ["Mailbox/get", { "state": "s2", "list": [{ "id": "x", "name": "New Folder", "role": null }] }, "1"],
            ["Mailbox/get", { "state": "s2", "list": [] }, "2"]
        ]
    });
    let sync = provider(vec![response])
        .sync_mailboxes(&account(), Some(&SyncState::new("s1")))
        .await
        .unwrap();
    assert!(!sync.is_snapshot());
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &sync.update
    else {
        panic!("expected delta");
    };
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].name, "New Folder");
    assert_eq!(removed.len(), 1);
    assert_eq!(sync.next_cursor.as_str(), "s2");
}

#[tokio::test]
async fn mailbox_resync_recovers_via_snapshot() {
    let error_changes =
        json!({ "methodResponses": [["error", { "type": "cannotCalculateChanges" }, "0"]] });
    let p = provider(vec![
        error_changes,
        fixture("mailbox_snapshot_response.json"),
    ]);
    let sync = p
        .sync_mailboxes(&account(), Some(&SyncState::new("stale")))
        .await
        .unwrap();
    assert!(sync.is_snapshot());
}

#[tokio::test]
async fn permanent_fetch_errors_propagate() {
    let err = || json!({ "methodResponses": [["error", { "type": "forbidden" }, "0"]] });
    let p = provider(vec![err(), err(), err(), err(), err()]);
    assert!(p.sync_mailboxes(&account(), None).await.is_err());
    assert!(p.sync_email(&account(), None).await.is_err());
    assert!(p.sync_calendars(&account(), None).await.is_err());
    assert!(p.sync_events(&account(), None).await.is_err());
    let draft = Draft::new(
        engine_core::ids::MessageIdHeader::new("m@h").unwrap(),
        engine_core::mail::EmailAddress::new("a@h"),
        vec![engine_core::mail::EmailAddress::new("b@h")],
        "s",
        "b",
    );
    assert!(p.submit_email(&account(), &draft).await.is_err());
}

#[tokio::test]
async fn missing_account_ids_surface_as_errors() {
    let bare = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(
        &json!({
            "capabilities": { "urn:ietf:params:jmap:core": {} },
            "primaryAccounts": {},
            "apiUrl": "https://mail.test.local/jmap/"
        }),
        vec![],
    )));
    assert!(bare.sync_mailboxes(&account(), None).await.is_err());
    assert!(bare.sync_email(&account(), None).await.is_err());
    assert!(bare.sync_calendars(&account(), None).await.is_err());
    assert!(bare.sync_events(&account(), None).await.is_err());
}

#[tokio::test]
async fn capabilities_and_scopes_come_from_the_session() {
    let p = provider(vec![]);
    assert!(p.connection_info().capabilities.mail());
    assert!(p.connection_info().capabilities.submission());
    assert!(p.connection_info().capabilities.calendars());
    assert_eq!(
        p.email_scope(&account()),
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        }
    );
    assert_eq!(
        p.mailbox_scope(&account()),
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Mailbox,
        }
    );
    assert_eq!(
        p.event_scope(&account()),
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::CalendarEvent,
        }
    );
}
