//! Shared offline harness for the provider sync/submission tests.
//!
//! A [`FakeExecutor`] replays canned JMAP response documents (real captured
//! server responses) with no socket, so the orchestration is exercised offline;
//! the free functions build providers and load fixtures for the sibling test
//! modules.
//!
//! It replies the same canned bytes **whatever it is sent**, so on its own it cannot catch
//! a malformed request — a `CalendarEvent/set` with the wrong method name or a bad patch
//! pointer would sail through (`AGENTS.md`). So it also **records every request** it was
//! given ([`FakeExecutor::requests`]), letting a test assert the exact wire envelope it
//! produced. That closes the gap for request *shape*; whether the server accepts it is what
//! the live Stalwart suite proves.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

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
    /// Every request the fake was handed, as the exact `{using, methodCalls}` JSON that
    /// would have gone on the wire — the only way an offline test can check a request
    /// *shape*, since the canned response is served regardless.
    pub(crate) requests: Mutex<Vec<Value>>,
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
                "urn:ietf:params:jmap:calendars": {},
                "urn:ietf:params:jmap:contacts": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c",
                "urn:ietf:params:jmap:calendars": "c",
                "urn:ietf:params:jmap:contacts": "c"
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
            requests: Mutex::new(Vec::new()),
            download_body: None,
            download_urls: Mutex::new(Vec::new()),
            upload_blob_ids: Mutex::new(VecDeque::new()),
            uploads: Mutex::new(Vec::new()),
        }
    }

    /// The single method call the fake was sent: `(using, method, arguments)`.
    ///
    /// Panics unless exactly one request carrying exactly one call was made — which is the
    /// contract every `*/set` write in this crate holds to.
    pub(crate) fn sole_call(&self) -> (Vec<String>, String, Value) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let using = requests[0]["using"]
            .as_array()
            .expect("using")
            .iter()
            .map(|u| u.as_str().expect("urn").to_owned())
            .collect();
        let calls = requests[0]["methodCalls"].as_array().expect("methodCalls");
        assert_eq!(calls.len(), 1, "expected exactly one method call");
        (
            using,
            calls[0][0].as_str().expect("method").to_owned(),
            calls[0][1].clone(),
        )
    }

    /// How many requests the fake was handed.
    ///
    /// `sole_call` asserts exactly one; this is for the opposite claim — that a refusal
    /// happened *before* the network, so nothing was half-applied.
    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        // Record the exact envelope before serving the canned reply: the reply does not
        // depend on it, so this is the only offline evidence of what we actually sent.
        self.requests.lock().unwrap().push(request.to_json());
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

/// A provider over a fake the caller **keeps a handle to**, so a test can assert the
/// requests it produced — the only way to check a request shape offline, since the canned
/// response is served regardless of what was sent.
pub(crate) fn recording(responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::new(responses));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

/// Lets a test hold the fake (to read its recordings) while the provider owns it too.
#[async_trait]
impl Executor for Arc<FakeExecutor> {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        (**self).execute(request).await
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        (**self).download(url).await
    }

    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError> {
        (**self).upload(url, media_type, bytes).await
    }

    fn session(&self) -> &Session {
        (**self).session()
    }
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
