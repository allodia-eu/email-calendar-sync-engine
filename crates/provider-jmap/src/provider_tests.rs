use std::collections::VecDeque;
use std::sync::Mutex;

use reqwest::Url;

use super::*;
use crate::session::SessionUrlPolicy;
use engine_core::sync::SyncUpdate;
use engine_provider::SyncKind;
use serde_json::{Value, json};

/// An executor that replays canned response documents, FIFO — driving the
/// orchestration with real captured Stalwart responses, offline. It also serves a
/// canned blob-download body and records the download URLs it was asked for, so the
/// `fetch_message_source` template substitution can be asserted without a server.
struct FakeExecutor {
    session: Session,
    responses: Mutex<VecDeque<Response>>,
    download_body: Option<Vec<u8>>,
    download_urls: Mutex<Vec<String>>,
    /// Canned `blobId`s returned by successive `upload` calls (FIFO); records the
    /// (url, media_type, bytes) it was asked to upload for assertions.
    upload_blob_ids: Mutex<VecDeque<String>>,
    uploads: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl FakeExecutor {
    fn new(responses: Vec<Value>) -> Self {
        let session_doc = json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": { "maxObjectsInGet": 500 },
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {},
                "urn:ietf:params:jmap:calendars": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c",
                "urn:ietf:params:jmap:calendars": "c"
            },
            "apiUrl": "https://mail.test.local/jmap/",
            "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}",
            "uploadUrl": "https://mail.test.local/upload/{accountId}/"
        });
        Self::from_session(&session_doc, responses)
    }

    fn from_session(session_doc: &Value, responses: Vec<Value>) -> Self {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let session =
            Session::parse(session_doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
        let parsed = responses
            .into_iter()
            .map(|v| Response::parse(&v).unwrap())
            .collect();
        Self {
            session,
            responses: Mutex::new(parsed),
            download_body: None,
            download_urls: Mutex::new(Vec::new()),
            upload_blob_ids: Mutex::new(VecDeque::new()),
            uploads: Mutex::new(Vec::new()),
        }
    }

    /// Serves `body` as the blob-download response for `fetch_message_source`.
    fn with_download_body(mut self, body: &[u8]) -> Self {
        self.download_body = Some(body.to_vec());
        self
    }

    /// Serves `blob_ids` (FIFO) as the results of successive `upload` calls.
    fn with_upload_blob_ids(self, blob_ids: impl IntoIterator<Item = &'static str>) -> Self {
        *self.upload_blob_ids.lock().unwrap() = blob_ids.into_iter().map(str::to_owned).collect();
        self
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn execute(&self, _request: &Request) -> Result<Response, JmapError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| JmapError::protocol("fake executor exhausted"))
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        self.download_urls.lock().unwrap().push(url.to_owned());
        self.download_body
            .clone()
            .ok_or_else(|| JmapError::status(404, "no blob"))
    }

    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError> {
        self.uploads
            .lock()
            .unwrap()
            .push((url.to_owned(), media_type.to_owned(), bytes.to_vec()));
        self.upload_blob_ids
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| JmapError::status(413, "no upload slot"))
    }

    fn session(&self) -> &Session {
        &self.session
    }
}

fn provider(responses: Vec<Value>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(FakeExecutor::new(responses)))
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// A minimal synced message carrying `blob` as its raw-source blob handle.
fn message_with_blob(id: &str, blob: &str) -> engine_core::mail::Message {
    use engine_core::ids::{BlobId, MailboxId, MessageId};
    use engine_core::membership::Memberships;
    let mut message = engine_core::mail::Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    message.blob_id = Some(BlobId::try_from(blob).unwrap());
    message
}

#[tokio::test]
async fn message_source_downloads_the_blob_and_substitutes_the_template() {
    const MIME: &[u8] = b"From: a@example.com\r\nSubject: Hi\r\n\r\nBody text\r\n";
    let exec = FakeExecutor::new(vec![]).with_download_body(MIME);
    let raw = crate::fetch::message_source(&exec, &message_with_blob("m1", "blob-1"))
        .await
        .unwrap();
    assert_eq!(raw.as_bytes(), MIME);
    // The download template's origin was rebased to the connection and every
    // placeholder substituted (mail account `c`, the message's blob id).
    assert_eq!(
        exec.download_urls.lock().unwrap().as_slice(),
        ["http://127.0.0.1:18080/download/c/blob-1/message?accept=application/octet-stream"]
    );
}

#[tokio::test]
async fn fetch_message_source_provider_method_returns_the_raw_mime() {
    let exec = FakeExecutor::new(vec![]).with_download_body(b"raw-bytes");
    let provider = JmapProvider::with_executor(Box::new(exec));
    // Advertises the capability now that a download template is present.
    assert!(provider.capabilities().message_source());
    let raw = provider
        .fetch_message_source(&account(), &message_with_blob("m1", "b1"))
        .await
        .unwrap();
    assert_eq!(raw.as_bytes(), b"raw-bytes");
}

#[tokio::test]
async fn message_source_without_a_blob_id_is_a_protocol_error() {
    use engine_core::ids::{MailboxId, MessageId};
    use engine_core::membership::Memberships;
    let exec = FakeExecutor::new(vec![]).with_download_body(b"x");
    let message = engine_core::mail::Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    let err = crate::fetch::message_source(&exec, &message)
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
    // Three emails over two pages of two: page one hands back a continuation
    // token; page two, a short page, completes the pass.
    let p = provider(vec![
        email_query_page(&["e1", "e2"], 3),
        email_query_page(&["e3"], 3),
    ]);

    let page1 = p.sync_email_page(&account(), None, None, 2).await.unwrap();
    assert_eq!(page1.kind, SyncKind::Snapshot);
    assert_eq!(page1.total, Some(3));
    assert_eq!(page1.changed.len(), 2);
    assert_eq!(page1.present.len(), 2);
    let token = page1.next_page.expect("a full first page implies more");

    // The opaque token resumes the pass at the next position.
    let page2 = p
        .sync_email_page(&account(), None, Some(&token), 2)
        .await
        .unwrap();
    assert_eq!(page2.changed.len(), 1);
    assert_eq!(page2.present.len(), 1);
    assert!(page2.next_page.is_none(), "the snapshot pass is complete");
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

    let first = p.sync_email_page(&account(), None, None, 2).await.unwrap();
    assert_eq!(first.total, None);
    let token = first
        .next_page
        .expect("a full page implies more when no total is known");

    let second = p
        .sync_email_page(&account(), None, Some(&token), 2)
        .await
        .unwrap();
    assert!(second.next_page.is_none(), "a short page ends the pass");
    assert_eq!(second.changed.len(), 1);
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

    let first = p
        .sync_email_page(&account(), Some(&SyncState::new("s1")), None, 1)
        .await
        .unwrap();
    assert_eq!(first.kind, SyncKind::Delta);
    assert_eq!(first.changed.len(), 1);
    let token = first.next_page.expect("hasMoreChanges → another page");

    let second = p
        .sync_email_page(&account(), Some(&SyncState::new("s1")), Some(&token), 1)
        .await
        .unwrap();
    assert!(second.next_page.is_none(), "no more changes");
    assert_eq!(second.removed.len(), 1);
    assert_eq!(second.next_cursor.as_str(), "s3");
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
    let SyncUpdate::Delta { changed, removed } = &sync.update else {
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
async fn submit_email_resolves_context_then_sends() {
    use engine_core::ids::MessageIdHeader;
    use engine_core::mail::EmailAddress;
    use engine_provider::Draft;

    // Two requests: resolve Drafts/Sent + identity, then create + submit.
    let p = provider(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ]);
    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Step 4 submission probe",
        "Hello",
    );
    let receipt = p.submit_email(&account(), &draft).await.unwrap();
    assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
    assert_eq!(
        receipt.message_id.as_str(),
        "step4-send-probe-0002@test.local"
    );
}

#[tokio::test]
async fn submit_email_uploads_attachment_bytes_before_sending() {
    use engine_core::ids::MessageIdHeader;
    use engine_core::mail::EmailAddress;
    use engine_provider::{Draft, DraftAttachment};

    // The upload endpoint hands back a blobId, then the two-step send proceeds. Drive
    // `submit::send` directly so the fake's recorded uploads can be inspected after.
    let exec = FakeExecutor::new(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ])
    .with_upload_blob_ids(["blob-att-1"]);

    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "With attachment",
        "See attached.",
    )
    .with_attachment(DraftAttachment::attachment(
        "report.pdf",
        "application/pdf",
        vec![9, 8, 7],
    ));
    crate::submit::send(&exec, "c", "c", &draft).await.unwrap();

    // The attachment bytes were POSTed to the resolved (account-substituted) upload URL
    // with the right media type — before the Email/set that references the blob.
    let uploads = exec.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "http://127.0.0.1:18080/upload/c/");
    assert_eq!(uploads[0].1, "application/pdf");
    assert_eq!(uploads[0].2, vec![9, 8, 7]);
}

#[tokio::test]
async fn submit_with_attachment_but_no_upload_url_is_a_session_error() {
    use engine_core::error::FailureClass;
    use engine_core::ids::MessageIdHeader;
    use engine_core::mail::EmailAddress;
    use engine_provider::{Draft, DraftAttachment, Provider};

    // A server without an uploadUrl cannot take attachments — a clear, permanent error.
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(
        &json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c"
            },
            "apiUrl": "https://mail.test.local/jmap/"
        }),
        vec![],
    )));
    let draft = Draft::new(
        MessageIdHeader::new("m@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "x",
        "y",
    )
    .with_attachment(DraftAttachment::attachment(
        "r.pdf",
        "application/pdf",
        vec![1],
    ));
    let err = p.submit_email(&account(), &draft).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
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
    let SyncUpdate::Delta { changed, removed } = &sync.update else {
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
async fn edit_mail_marks_seen_through_the_real_set_flow() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    // A writable mail account advertises mail writes.
    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "updated": { "eaaaaab": null } }, "0"]]
    })]);
    assert!(p.capabilities().mail_writes());

    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .unwrap();
    // The JMAP id is stable across the edit — the receipt echoes it.
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_delete_destroys_via_set() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "destroyed": ["eaaaaab"] }, "0"]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::delete(key.clone()))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_set_error_surfaces_as_a_conflict() {
    use engine_core::error::FailureClass;
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    // The target was destroyed server-side since it synced: a `notFound` SetError.
    let p = provider(vec![json!({
        "methodResponses": [[
            "Email/set",
            { "notUpdated": { "eaaaaab": { "type": "notFound" } } },
            "0"
        ]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let err = p
        .edit_mail(&account(), &MailEdit::set_flagged(key, true))
        .await
        .unwrap_err();
    // Conflict → the caller re-syncs (tombstoning the gone message), then retries.
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn capabilities_and_scopes_come_from_the_session() {
    let p = provider(vec![]);
    assert!(p.capabilities().mail());
    assert!(p.capabilities().submission());
    assert!(p.capabilities().calendars());
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
