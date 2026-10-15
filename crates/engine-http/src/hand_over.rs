//! The point of no return of an HTTP submission: the last piece of its body.
//!
//! A server acts on a request only once it has all of it, and the body is what it waits for:
//! the head alone (method, path, `Content-Length`) delivers nothing. So the line a submission
//! crosses is the last piece of the body, and the hand-over is recorded immediately before it.
//! Everything earlier is safe to repeat: the name lookup, the connect, the TLS handshake, the
//! head and every piece but the last.
//!
//! reqwest asks for the body only on a connection that is established, so this also puts a
//! process that ends while connecting, or while a large attachment is still going up, on the
//! side of the line where the send is simply retried.
//!
//! The record is the outbox's, which the body cannot reach (reqwest wants a `'static` body).
//! So the body holds a [`Gate`] and parks on it, and the funnel that sends the request, which
//! holds the [`HandOver`], commits it when the gate asks and opens or refuses the gate.

use core::{
    convert::Infallible,
    task::{Context, Poll, Waker},
};
use std::sync::Mutex;

use engine_provider::HandOver;
use tokio::sync::Notify;

/// Where a submission's body waits for its hand-over to be recorded.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    state: Mutex<State>,
    asked: Notify,
}

#[derive(Debug, Default)]
enum State {
    /// The body has not reached its last piece, or is parked on it.
    #[default]
    Closed,
    /// The body is parked on its last piece, waiting for the hand-over.
    Asked(Waker),
    /// The hand-over is recorded: the last piece may go.
    Open,
    /// The hand-over could not be recorded, and why: the last piece never goes.
    Refused(String),
}

/// The error a body ends with when its hand-over was refused. The request is abandoned short
/// of its end, which a server cannot act on.
#[derive(Debug)]
pub(crate) struct Withheld;

impl core::fmt::Display for Withheld {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the end of the submission was withheld")
    }
}

impl std::error::Error for Withheld {}

impl Gate {
    /// Called by the body before its last piece: ready once the hand-over is recorded.
    pub(crate) fn poll_open(&self, cx: &mut Context<'_>) -> Poll<Result<(), Withheld>> {
        let mut state = self.state.lock().expect("hand-over gate poisoned");
        match &*state {
            State::Open => Poll::Ready(Ok(())),
            State::Refused(_) => Poll::Ready(Err(Withheld)),
            State::Closed => {
                *state = State::Asked(cx.waker().clone());
                self.asked.notify_one();
                Poll::Pending
            }
            State::Asked(_) => {
                *state = State::Asked(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Why the hand-over was refused, once it has been.
    pub(crate) fn refusal(&self) -> Option<String> {
        match &*self.state.lock().expect("hand-over gate poisoned") {
            State::Refused(detail) => Some(detail.clone()),
            _ => None,
        }
    }

    fn settle(&self, outcome: State) {
        let previous = core::mem::replace(
            &mut *self.state.lock().expect("hand-over gate poisoned"),
            outcome,
        );
        if let State::Asked(waker) = previous {
            waker.wake();
        }
    }

    /// Commits `hand_over` when the body asks, and opens or refuses the gate. Never returns:
    /// it runs beside the request and is dropped with it.
    pub(crate) async fn drive(&self, hand_over: &HandOver<'_>) -> Infallible {
        self.asked.notified().await;
        match hand_over.commit().await {
            Ok(_proof) => self.settle(State::Open),
            Err(err) => self.settle(State::Refused(err.detail().to_owned())),
        }
        core::future::pending().await
    }
}

#[cfg(test)]
#[path = "hand_over_tests.rs"]
mod tests;
