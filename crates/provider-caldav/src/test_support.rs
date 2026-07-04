//! Shared test fakes for the offline suites (one place instead of a copy per
//! module). Compiled only under `cfg(test)`.

use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::error::CalDavError;
use crate::transport::{DavExecutor, DavMethod, HttpResponse, WriteRequest};

/// A fake [`DavExecutor`] that replays canned responses in request order and
/// records each request's `(method, href)` for assertions. Reads and writes draw
/// from the **same** response queue, so a mixed read/write flow replays in order;
/// each write's full [`WriteRequest`] is captured for header/body assertions.
pub(crate) struct Replay {
    responses: Mutex<Vec<HttpResponse>>,
    seen: Mutex<Vec<(DavMethod, String)>>,
    writes: Mutex<Vec<WriteRequest>>,
}

impl Replay {
    /// Replays the given responses, in order.
    pub(crate) fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            seen: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
        }
    }

    /// Replays each body as a `207 Multi-Status` response, in order.
    pub(crate) fn bodies(bodies: Vec<&str>) -> Self {
        Self::new(bodies.into_iter().map(ok).collect())
    }

    /// The `(method, href)` of each request received so far.
    pub(crate) fn seen(&self) -> MutexGuard<'_, Vec<(DavMethod, String)>> {
        self.seen.lock().expect("seen lock")
    }

    /// The full [`WriteRequest`] of each write (`PUT`/`DELETE`) received so far.
    pub(crate) fn writes(&self) -> MutexGuard<'_, Vec<WriteRequest>> {
        self.writes.lock().expect("writes lock")
    }
}

#[async_trait]
impl DavExecutor for Replay {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        _depth: &str,
        _body: String,
    ) -> Result<HttpResponse, CalDavError> {
        self.seen
            .lock()
            .expect("seen lock")
            .push((method, href.to_owned()));
        Ok(self.responses.lock().expect("responses lock").remove(0))
    }

    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError> {
        self.seen
            .lock()
            .expect("seen lock")
            .push((request.method, request.href.clone()));
        self.writes.lock().expect("writes lock").push(request);
        Ok(self.responses.lock().expect("responses lock").remove(0))
    }
}

/// Lets a test keep a shared handle to the fake after it is moved into a provider
/// (which takes an owned `Box<dyn DavExecutor>`): clone the `Arc` into the provider
/// and inspect [`Replay::writes`]/[`Replay::seen`] afterwards.
#[async_trait]
impl DavExecutor for std::sync::Arc<Replay> {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        (**self).send(method, href, depth, body).await
    }

    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError> {
        (**self).send_write(request).await
    }
}

/// A `207 Multi-Status` response carrying `body`.
pub(crate) fn ok(body: &str) -> HttpResponse {
    HttpResponse {
        status: 207,
        body: body.to_owned(),
        location: None,
        etag: None,
    }
}

/// A write response: `status`, an optional new `ETag` header, no body.
pub(crate) fn wrote(status: u16, etag: Option<&str>) -> HttpResponse {
    HttpResponse {
        status,
        body: String::new(),
        location: None,
        etag: etag.map(str::to_owned),
    }
}
