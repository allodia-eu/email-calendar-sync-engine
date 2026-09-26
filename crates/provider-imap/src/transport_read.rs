//! Framing: reading one logical line of a response off the wire.
//!
//! Split from [`crate::transport`], which owns the command round trip, because this is the
//! part that decides how long to wait for a server and how much of what it announces to
//! believe.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    transport::Connection,
};

/// The largest `{n}` literal we will read into memory. A hostile or buggy server
/// could announce an enormous literal (`* {4000000000}`); the cap bounds the
/// allocation so adversarial input cannot exhaust memory (`north-star.md` security).
/// Generous enough for any real metadata response (and future body fetches).
const MAX_LITERAL: usize = 64 * 1024 * 1024;

/// How long a body read may go without a single byte from the server before its connection is
/// given up for dead.
///
/// A bound on silence rather than on the whole read, because a body is one literal that can run
/// to tens of megabytes over a slow link, and it arrives steadily however long it takes. A
/// socket whose path died mid-response (a phone changing radio, a NAT dropping the mapping)
/// sends nothing and reports nothing, and holds the read until the OS gives up retransmitting,
/// which is many minutes. A server answers a `UID FETCH` in milliseconds, so a minute of nothing
/// is not a slow server.
pub(crate) const BODY_READ_STALL: Duration = Duration::from_secs(60);

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Reads one logical line: bytes through the next `\n`, with any `{n}` literal
    /// the line announces inlined (the n bytes, then the continuation). Literals
    /// can themselves announce further literals, so this loops.
    ///
    /// `pub(crate)` so the IDLE read loop ([`crate::idle`]) can consume the
    /// unsolicited untagged responses the server streams while idling, reusing the
    /// same literal-aware framing as the command path.
    pub(crate) async fn read_line(&mut self) -> ImapResult<Vec<u8>> {
        self.read_line_within(None).await
    }

    /// [`read_line`](Self::read_line), failing with [`std::io::ErrorKind::TimedOut`] once
    /// `stall` passes without a byte arriving. A literal is read in pieces so that the bound
    /// is on each wait, never on the whole literal.
    pub(crate) async fn read_line_within(&mut self, stall: Option<Duration>) -> ImapResult<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let before = line.len();
            let read = within(stall, self.inner.read_until(b'\n', &mut line)).await?;
            if read == 0 {
                return Err(closed_mid_response());
            }
            if let Some(len) = trailing_literal_len(&line[before..]) {
                if len > MAX_LITERAL {
                    return Err(ImapError::protocol(format!(
                        "server announced a {len}-byte literal exceeding the {MAX_LITERAL}-byte cap"
                    )));
                }
                let start = line.len();
                line.resize(start + len, 0);
                let mut filled = start;
                while filled < line.len() {
                    let read = within(stall, self.inner.read(&mut line[filled..])).await?;
                    if read == 0 {
                        return Err(closed_mid_response());
                    }
                    filled += read;
                }
                continue;
            }
            return Ok(line);
        }
    }

}

/// Awaits one read, bounded by `stall` when there is one.
async fn within<T>(
    stall: Option<Duration>,
    read: impl Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    let Some(limit) = stall else {
        return read.await;
    };
    tokio::time::timeout(limit, read).await.unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("the server sent nothing for {}s", limit.as_secs()),
        ))
    })
}

fn closed_mid_response() -> ImapError {
    ImapError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "connection closed mid-response",
    ))
}

/// The literal length a line announces (`…{n}` or `…{n+}` before its CRLF), if any.
fn trailing_literal_len(line: &[u8]) -> Option<usize> {
    let trimmed = line.strip_suffix(b"\n")?;
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    let inside = trimmed.strip_suffix(b"}")?;
    let inside = inside.strip_suffix(b"+").unwrap_or(inside);
    let open = inside.iter().rposition(|&b| b == b'{')?;
    let digits = &inside[open + 1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

