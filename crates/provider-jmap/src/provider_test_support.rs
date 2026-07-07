//! Shared offline harness for the provider sync/submission tests.
//!
//! A [`FakeExecutor`] replays canned JMAP response documents (real captured
//! server responses) with no socket, so the orchestration is exercised offline;
//! the free functions build providers and load fixtures for the sibling test
//! modules.

use std::{collections::VecDeque, sync::Mutex};

use reqwest::Url;
use serde_json::{Value, json};

use super::*;
use crate::session::SessionUrlPolicy;

/// An executor that replays canned response documents, FIFO — driving the
/// orchestration with real captured Stalwart responses, offline. It also serves a
/// canned blob-download body and records the download URLs it was asked for, so the
/// `fetch_message_source` template substitution can be asserted without a server.
pub(crate) struct FakeExecutor {
    session: Session,
    responses: Mutex<VecDeque<Response>>,
    download_body: Option<Vec<u8>>,
    pub(crate) download_urls: Mutex<Vec<String>>,
    /// Canned `blobId`s returned by successive `upload` calls (FIFO); records the
    /// (url, media_type, bytes) it was asked to upload for assertions.
    upload_blob_ids: Mutex<VecDeque<String>>,
    pub(crate) uploads: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl FakeExecutor {
    pub(crate) fn new(responses: Vec<Value>) -> Self {
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

    pub(crate) fn from_session(session_doc: &Value, responses: Vec<Value>) -> Self {
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
    pub(crate) fn with_download_body(mut self, body: &[u8]) -> Self {
        self.download_body = Some(body.to_vec());
        self
    }

    /// Serves `blob_ids` (FIFO) as the results of successive `upload` calls.
    pub(crate) fn with_upload_blob_ids(
        self,
        blob_ids: impl IntoIterator<Item = &'static str>,
    ) -> Self {
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

pub(crate) fn provider(responses: Vec<Value>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(FakeExecutor::new(responses)))
}

pub(crate) fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

pub(crate) fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// A minimal synced message carrying `blob` as its raw-source blob handle.
pub(crate) fn message_with_blob(id: &str, blob: &str) -> engine_core::mail::Message {
    use engine_core::{
        ids::{BlobId, MailboxId, MessageId},
        membership::Memberships,
    };
    let mut message = engine_core::mail::Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    message.blob_id = Some(BlobId::try_from(blob).unwrap());
    message
}
