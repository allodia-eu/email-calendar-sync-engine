//! An executor for the import's offline tests: it answers each request and each upload from
//! its script, stores whatever it is sent once the script runs out, and records what it was
//! sent, how many uploads ran at once included.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, BlobId, MailboxId},
    membership::Memberships,
    raw::RawMime,
};
use engine_provider::import::MessageImport;
use reqwest::Url;
use serde_json::{Value, json};

use super::*;
use crate::{
    provider::provider_test_support::fixture,
    request::Request,
    response::Response,
    session::{Session, SessionUrlPolicy},
};

/// An executor answering each request and each upload from its script, in order.
pub(crate) struct Scripted {
    pub(crate) session: Session,
    pub(crate) replies: Mutex<VecDeque<Result<Value, JmapError>>>,
    pub(crate) uploads: Mutex<VecDeque<Result<String, JmapError>>>,
    pub(crate) sent: Mutex<Vec<Value>>,
    pub(crate) uploaded: Mutex<Vec<(String, String, usize)>>,
    /// Uploads in flight now, and the most there ever were at once.
    pub(crate) in_flight: AtomicUsize,
    pub(crate) peak: AtomicUsize,
}

#[async_trait]
impl Executor for Arc<Scripted> {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        let wire = request.to_json();
        self.sent.lock().unwrap().push(wire.clone());
        // Past its script, it stores whatever it is sent.
        let reply = self.replies.lock().unwrap().pop_front();
        Response::parse(&reply.unwrap_or_else(|| Ok(mirrored(&wire)))?)
    }

    async fn download(&self, _: &str) -> Result<Vec<u8>, JmapError> {
        unreachable!("an import downloads nothing")
    }

    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<BlobId, JmapError> {
        self.uploaded
            .lock()
            .unwrap()
            .push((url.to_owned(), media_type.to_owned(), bytes.len()));
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        // Stay in flight across a few polls, so uploads the importer runs at once overlap.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let scripted = self
            .uploads
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted")?;
        Ok(BlobId::try_from(scripted.as_str()).expect("a scripted blob id"))
    }

    fn session(&self) -> &Session {
        &self.session
    }
}

/// The answer of a server that stores every email `request` names.
pub(crate) fn mirrored(request: &Value) -> Value {
    let calls = request["methodCalls"].as_array().unwrap();
    let answers: Vec<Value> = calls
        .iter()
        .map(|call| {
            let created: serde_json::Map<String, Value> = call[1]
                .get("create")
                .or_else(|| call[1].get("emails"))
                .and_then(Value::as_object)
                .unwrap()
                .keys()
                .map(|id| (id.clone(), json!({ "id": id.to_uppercase() })))
                .collect();
            json!([call[0], { "created": created }, call[2]])
        })
        .collect();
    json!({ "methodResponses": answers })
}

/// Stalwart's captured session for the account `c`, with `edit` applied to it.
pub(crate) fn session(edit: impl FnOnce(&mut Value)) -> Session {
    let mut doc = fixture("session_shared_accounts.json");
    edit(&mut doc);
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection, None).unwrap()
}

pub(crate) fn target(
    session: Session,
    replies: Vec<Result<Value, JmapError>>,
    uploads: Vec<Result<String, JmapError>>,
) -> (JmapProvider, Arc<Scripted>) {
    let scripted = Arc::new(Scripted {
        session,
        replies: Mutex::new(replies.into()),
        uploads: Mutex::new(uploads.into()),
        sent: Mutex::default(),
        uploaded: Mutex::default(),
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    (
        JmapProvider::with_executor(Box::new(scripted.clone())),
        scripted,
    )
}

pub(crate) fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

pub(crate) fn message(len: usize, mailboxes: usize) -> MessageImport {
    MessageImport::new(
        RawMime::new(vec![b'x'; len]),
        Memberships::new(
            (0..mailboxes).map(|n| MailboxId::try_from(format!("mb{n}").as_str()).unwrap()),
        )
        .unwrap(),
    )
}

/// An `Email/import` answer creating every one of `indexes` as `M<n>`, after an upload of each
/// that went inline.
pub(crate) fn stored(indexes: &[usize], inline: &[usize]) -> Value {
    let blobs: serde_json::Map<String, Value> = inline
        .iter()
        .map(|n| (format!("b{n}"), json!({ "id": format!("B{n}") })))
        .collect();
    let emails: serde_json::Map<String, Value> = indexes
        .iter()
        .map(|n| (format!("m{n}"), json!({ "id": format!("M{n}") })))
        .collect();
    let import =
        json!(["Email/import", { "created": emails }, if inline.is_empty() { "0" } else { "1" }]);
    let answers = if inline.is_empty() {
        json!([import])
    } else {
        json!([["Blob/upload", { "created": blobs }, "0"], import])
    };
    json!({ "methodResponses": answers })
}

pub(crate) fn key(answer: &ProviderResult<ImportedMessage>) -> &str {
    answer.as_ref().unwrap().key.as_ref().unwrap().as_str()
}
