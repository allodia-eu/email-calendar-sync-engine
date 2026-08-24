//! The execution seam: one trait for "send this batched JMAP request", implemented by the
//! live client and, in tests, by a fake fed captured response documents.
//!
//! Its own module rather than part of [`provider`](crate::provider) because it is what makes
//! the whole adapter testable offline: every fetch, write and submission path is written
//! against this trait, so the orchestration is exercised against real Stalwart bytes with no
//! network. The provider is one of its callers, not its owner.

use async_trait::async_trait;
use engine_provider::HttpVersion;

use crate::{
    JmapClient,
    error::JmapError,
    request::{Request, Response},
    session::Session,
};

/// Executes a batched JMAP request and exposes the session.
///
/// Implemented by the live [`JmapClient`] and, in tests, by a fake fed canned
/// response documents — so the sync orchestration is fully exercised offline.
#[async_trait]
pub(crate) trait Executor: Send + Sync {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError>;
    /// GETs raw bytes from a resolved blob-download URL (the raw message source).
    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError>;
    /// POSTs raw `bytes` of `media_type` to a resolved blob-upload URL, returning the
    /// server-assigned `blobId` (RFC 8620 §6.1) — used to attach a draft's parts.
    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError>;
    fn session(&self) -> &Session;
    /// The HTTP version the transport negotiated. Defaults to `None`: only the live
    /// [`JmapClient`] speaks HTTP, so a fake fed canned documents has no version to
    /// report.
    fn http_version(&self) -> Option<HttpVersion> {
        None
    }
}

#[async_trait]
impl Executor for JmapClient {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        JmapClient::execute(self, request).await
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, JmapError> {
        JmapClient::download(self, url).await
    }

    async fn upload(&self, url: &str, media_type: &str, bytes: &[u8]) -> Result<String, JmapError> {
        JmapClient::upload(self, url, media_type, bytes).await
    }

    fn session(&self) -> &Session {
        JmapClient::session(self)
    }

    fn http_version(&self) -> Option<HttpVersion> {
        JmapClient::http_version(self)
    }
}
