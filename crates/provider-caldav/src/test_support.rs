//! Shared test fakes for the offline suites (one place instead of a copy per
//! module). Compiled only under `cfg(test)`.

use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::error::CalDavError;
use crate::transport::{DavExecutor, DavMethod, HttpResponse};

/// A fake [`DavExecutor`] that replays canned responses in request order and
/// records each request's `(method, href)` for assertions.
pub(crate) struct Replay {
    responses: Mutex<Vec<HttpResponse>>,
    seen: Mutex<Vec<(DavMethod, String)>>,
}

impl Replay {
    /// Replays the given responses, in order.
    pub(crate) fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            seen: Mutex::new(Vec::new()),
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
}

/// A `207 Multi-Status` response carrying `body`.
pub(crate) fn ok(body: &str) -> HttpResponse {
    HttpResponse {
        status: 207,
        body: body.to_owned(),
        location: None,
    }
}
