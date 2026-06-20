//! A scripted in-memory async stream for offline transport/sync/provider tests.
//!
//! It serves pre-canned server bytes to reads and records everything the client
//! writes, so the full IMAP (and SMTP) protocol can be driven with no socket and no
//! TLS — the same fidelity as `provider-jmap`'s fake executor, exercising the real
//! parsers and command sequencing against captured transcripts.

use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A handle to the bytes the client wrote, for asserting the commands it issued.
pub(crate) type Recorded = Arc<Mutex<Vec<u8>>>;

/// An async stream backed by a fixed server script (read side) and a recording
/// buffer (write side).
pub(crate) struct MockStream {
    to_client: io::Cursor<Vec<u8>>,
    from_client: Recorded,
}

impl MockStream {
    /// Builds a stream that serves `server_script` and records writes. The returned
    /// [`Recorded`] handle exposes the client's bytes after the run.
    pub(crate) fn new(server_script: impl Into<Vec<u8>>) -> (Self, Recorded) {
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let stream = Self {
            to_client: io::Cursor::new(server_script.into()),
            from_client: Arc::clone(&recorded),
        };
        (stream, recorded)
    }
}

impl AsyncRead for MockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let mut scratch = vec![0u8; buf.remaining()];
        let read = me.to_client.read(&mut scratch).unwrap_or(0);
        buf.put_slice(&scratch[..read]);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for MockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .from_client
            .lock()
            .expect("mock write lock")
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Concatenates response fragments verbatim into one server script (each fragment
/// supplies its own CRLFs, so literal payloads pass through untouched).
pub(crate) fn script(parts: &[&str]) -> Vec<u8> {
    parts.concat().into_bytes()
}

/// Returns the recorded client bytes as a UTF-8 string for command assertions.
pub(crate) fn written(recorded: &Recorded) -> String {
    String::from_utf8_lossy(&recorded.lock().expect("mock read lock")).into_owned()
}
