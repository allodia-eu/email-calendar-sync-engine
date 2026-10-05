//! How long an HTTP exchange waits on a server that has stopped answering.
//!
//! The bounds are the shared [`Deadlines`]; this module applies them to reqwest, phase by
//! phase, so a large transfer that keeps moving is never cut off:
//!
//! | Phase | Bound | Where |
//! |---|---|---|
//! | TCP connect and TLS handshake | [`dial`](Deadlines::dial) | the client [`client`] builds |
//! | each piece of a request body | [`stall`](Deadlines::stall) | `send_bounded` |
//! | the response head, once the request is out | [`reply`](Deadlines::reply), or [`submission`](Deadlines::submission) for an [`Exchange::Submission`] | `send_bounded` |
//! | each read of a response body | [`stall`](Deadlines::stall) | [`chunk_within`] |
//!
//! reqwest has a `read_timeout`, and it is not used: it starts with the request and is not
//! reset while the body goes out, so it would cut off a slow upload of a large attachment.
//!
//! The request body is handed to reqwest in pieces, and every piece reqwest takes is
//! progress. That is also how a failure knows whether the request may have been received: a
//! server that stopped taking the body before its end cannot have acted on it.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use engine_provider::{Deadlines, HandOver};
use http_body::{Frame, SizeHint};
use tokio::time::Instant;

use crate::{
    error::{Reached, SendError},
    hand_over::{Gate, Withheld},
};

/// What a request is, which decides how long its reply may take once it has been sent.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Exchange<'a> {
    /// Any request that does not submit a message: its reply gets
    /// [`Deadlines::reply`].
    Ordinary,
    /// A request that submits a message for delivery (a JMAP `EmailSubmission/set`, a Graph
    /// `sendMail`, a Gmail `messages.send`): its reply gets [`Deadlines::submission`],
    /// because a reply that never comes leaves the send ambiguous, and the last piece of its
    /// body goes only once the [`HandOver`] is committed (`hand_over.rs`).
    Submission(&'a HandOver<'a>),
}

impl Exchange<'_> {
    fn reply(self) -> Duration {
        match self {
            Self::Ordinary => Deadlines::STANDARD.reply(),
            Self::Submission(_) => Deadlines::STANDARD.submission(),
        }
    }
}

/// The reqwest client builder every HTTP adapter starts from: the host's trust policy, as
/// [`reqwest_builder`](engine_tls::TlsClientConfig::reqwest_builder) configures it, and the
/// [`dial`](Deadlines::dial) bound on the connect and the TLS handshake.
///
/// reqwest's connect timeout covers the name lookup, the TCP connect and the TLS handshake
/// together, and a failure there is a connect error, which [`SendError`] knows never reached
/// the server.
pub fn client(tls: &engine_tls::TlsClientConfig) -> reqwest::ClientBuilder {
    tls.reqwest_builder()
        .connect_timeout(Deadlines::STANDARD.dial())
}

/// How large a piece of a request body reqwest is handed at a time.
///
/// Small enough that a slow uplink moves one in seconds, so the stall bound is on a server
/// that has stopped taking the body, never on the size of the body.
pub(crate) const PIECE: usize = 16 * 1024;

#[cfg(any(test, feature = "test-server"))]
thread_local! {
    /// TEST BUILDS ONLY: how many steps replies have taken on this thread, a head arriving or a
    /// read of a body. `test_server` holds tokio's paused clock until the client has taken the
    /// step its bytes allow, because on the paused clock bytes still in the kernel lose to every
    /// timer.
    pub(crate) static PROGRESS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Counts a step a reply took, in a test build ([`PROGRESS`]).
fn stepped() {
    #[cfg(any(test, feature = "test-server"))]
    PROGRESS.with(|steps| steps.set(steps.get() + 1));
}

/// What a request has shown of its progress: when it last moved, and whether all of it has
/// been handed over.
#[derive(Debug)]
struct Progress {
    state: Mutex<State>,
}

#[derive(Debug, Clone, Copy)]
struct State {
    moved: Instant,
    whole: bool,
}

impl Progress {
    fn new(whole: bool) -> Self {
        Self {
            state: Mutex::new(State {
                moved: Instant::now(),
                whole,
            }),
        }
    }

    fn get(&self) -> State {
        *self.state.lock().expect("progress lock poisoned")
    }

    /// A piece was taken; `whole` once it was the last.
    fn moved(&self, whole: bool) {
        let mut state = self.state.lock().expect("progress lock poisoned");
        state.moved = Instant::now();
        state.whole |= whole;
    }

    fn reached(&self) -> Reached {
        if self.get().whole {
            Reached::Possibly
        } else {
            Reached::NotWhole
        }
    }
}

/// A request body handed over [`PIECE`] by piece, recording each piece reqwest takes.
///
/// Its size is exact, so reqwest sends the same `Content-Length` it would for the bytes. A
/// submission's body holds its last piece back until the [`Gate`] opens.
struct Pieces {
    rest: Bytes,
    progress: Arc<Progress>,
    gate: Option<Arc<Gate>>,
}

impl http_body::Body for Pieces {
    type Data = Bytes;
    type Error = Withheld;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Withheld>>> {
        let me = self.get_mut();
        if me.rest.is_empty() {
            return Poll::Ready(None);
        }
        if me.rest.len() <= PIECE
            && let Some(gate) = &me.gate
        {
            match gate.poll_open(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(withheld)) => return Poll::Ready(Some(Err(withheld))),
                Poll::Ready(Ok(())) => {}
            }
        }
        let piece = me.rest.split_to(me.rest.len().min(PIECE));
        me.progress.moved(me.rest.is_empty());
        Poll::Ready(Some(Ok(Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.rest.len() as u64)
    }
}

/// Puts `request`'s body in [`Pieces`], returning what will record its progress and, for a
/// body that holds its last piece behind `gate`, the gate.
///
/// A request with no body is whole from the start: its head goes out the moment the
/// connection is up, so its reply bound runs from the send, the connect included. That is
/// sound because the dial bound is the shorter of the two. A body reqwest cannot read as
/// bytes (a stream; no adapter sends one) is left as it is and counted whole, which can only
/// make a failure look received.
fn track(request: &mut reqwest::Request, gated: bool) -> (Arc<Progress>, Option<Arc<Gate>>) {
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .filter(|bytes| !bytes.is_empty())
        .map(Bytes::copy_from_slice);
    let Some(rest) = body else {
        return (Arc::new(Progress::new(true)), None);
    };
    let progress = Arc::new(Progress::new(false));
    let gate = gated.then(|| Arc::new(Gate::default()));
    *request.body_mut() = Some(reqwest::Body::wrap(Pieces {
        rest,
        progress: Arc::clone(&progress),
        gate: gate.clone(),
    }));
    (progress, gate)
}

/// Sends `request` and waits for the head of its response, giving up on a server that goes
/// quiet: [`stall`](Deadlines::stall) between pieces of the body, then the bound `exchange`
/// names for the reply. A submission's hand-over is committed before the last piece of its
/// body, or before the request at all when it has no body to hold back.
pub(crate) async fn send_bounded(
    client: &reqwest::Client,
    mut request: reqwest::Request,
    exchange: Exchange<'_>,
) -> Result<reqwest::Response, SendError> {
    let hand_over = match exchange {
        Exchange::Submission(hand_over) => Some(hand_over),
        Exchange::Ordinary => None,
    };
    let (progress, gate) = track(&mut request, hand_over.is_some());
    let gate = match (hand_over, gate) {
        (Some(hand_over), Some(gate)) => Some((hand_over, gate)),
        (Some(hand_over), None) => {
            hand_over
                .commit()
                .await
                .map_err(|err| SendError::withheld(err.detail().to_owned()))?;
            None
        }
        (None, _) => None,
    };
    let drive = async {
        match &gate {
            Some((hand_over, gate)) => gate.drive(hand_over).await,
            None => core::future::pending().await,
        }
    };
    let failed = |err: reqwest::Error| match gate.as_ref().and_then(|(_, gate)| gate.refusal()) {
        Some(detail) => SendError::withheld(detail),
        None => SendError::http(err, progress.reached()),
    };
    let head = if armed() {
        tokio::select! {
            biased;
            sent = client.execute(request) => sent.map_err(failed),
            silent = quiet(&progress, exchange.reply()) => Err(silent),
            never = drive => match never {},
        }
    } else {
        tokio::select! {
            biased;
            sent = client.execute(request) => sent.map_err(failed),
            never = drive => match never {},
        }
    };
    if head.is_ok() {
        stepped();
    }
    head
}

/// Resolves once `progress` has stood still for longer than its phase allows.
async fn quiet(progress: &Progress, reply: Duration) -> SendError {
    loop {
        let state = progress.get();
        let (bound, what) = if state.whole {
            (reply, "the server sent nothing")
        } else {
            (Deadlines::STANDARD.stall(), "the server took nothing")
        };
        let deadline = state.moved + bound;
        if Instant::now() >= deadline {
            return SendError::silent(what, bound, progress.reached());
        }
        tokio::time::sleep_until(deadline).await;
    }
}

/// Reads the next piece of `response`'s body, failing once `quiet` passes without one.
///
/// The read behind [`Sent::bytes`](crate::Sent::bytes) and [`Sent::text`](crate::Sent::text)
/// passes [`stall`](Deadlines::stall); a stream that is silent on purpose between events
/// passes its own keep-alive bound instead.
///
/// # Errors
///
/// [`SendError`], a timeout once `quiet` passes. The response head had arrived, so the
/// request is always one the server may have received.
pub async fn chunk_within(
    response: &mut reqwest::Response,
    quiet: Duration,
) -> Result<Option<Bytes>, SendError> {
    match tokio::time::timeout(quiet, response.chunk()).await {
        Ok(read) => {
            stepped();
            read.map_err(|err| SendError::http(err, Reached::Possibly))
        }
        Err(_) => Err(SendError::silent(
            "the server sent nothing",
            quiet,
            Reached::Possibly,
        )),
    }
}

/// Reads `response`'s body to the end, each read under [`stall`](Deadlines::stall).
pub(crate) async fn drain(response: &mut reqwest::Response) -> Result<Vec<u8>, SendError> {
    let mut body = Vec::new();
    loop {
        let chunk = if armed() {
            chunk_within(response, Deadlines::STANDARD.stall()).await?
        } else {
            let read = response.chunk().await;
            read.map_err(|err| SendError::http(err, Reached::Possibly))?
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        body.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
thread_local! {
    /// Set by the throttle suites (`send_test_support::client`), which run on tokio's paused
    /// clock against a real socket. There every wait for the socket moves the clock on to the
    /// next timer, whatever the socket had ready, so an armed bound would race each reply to
    /// its own deadline and fail a test about something else. Those suites test throttling;
    /// `deadline_tests` tests the bounds, holding the clock still wherever a server answers.
    pub(crate) static UNBOUNDED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Whether the bounds on a silent server apply, which is always outside the throttle suites.
fn armed() -> bool {
    #[cfg(test)]
    if UNBOUNDED.get() {
        return false;
    }
    true
}

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod tests;
