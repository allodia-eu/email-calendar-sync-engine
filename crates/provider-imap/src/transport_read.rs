//! Framing: reading one logical line of a response off the wire.
//!
//! Split from [`crate::transport`], which owns the command round trip, because this is the
//! part that decides how long to wait for a server and how much of what it announces to
//! believe.

use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite};

use crate::{
    deadline::{self, BOUNDS},
    error::{ImapError, ImapResult},
    transport::Connection,
};

/// The largest `{n}` literal we will read into memory. A hostile or buggy server
/// could announce an enormous literal (`* {4000000000}`); the cap bounds the
/// allocation so adversarial input cannot exhaust memory (`north-star.md` security).
/// It caps one literal only: an IMAP line as a whole is not capped, so a server that never sends
/// a line end can make a line grow without bound (a known gap, beyond this cap).
const MAX_LITERAL: usize = 64 * 1024 * 1024;

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Reads one logical line: bytes through the next `\n`, with any `{n}` literal
    /// the line announces inlined (the n bytes, then the continuation). Literals
    /// can themselves announce further literals, so this loops.
    ///
    /// Every wait is bounded by [`Deadlines::reply`](engine_provider::Deadlines::reply): a
    /// line is something a server owes. The one read that may wait without a bound is a
    /// connection in `IDLE`, which [`crate::idle`] makes with
    /// [`read_line_within`](Self::read_line_within) and the watch bounds by its keep-alive.
    pub(crate) async fn read_line(&mut self) -> ImapResult<Vec<u8>> {
        self.read_line_within(Some(BOUNDS.reply())).await
    }

    /// [`read_line`](Self::read_line), failing with [`std::io::ErrorKind::TimedOut`] once
    /// `stall` passes without a byte arriving, or never when it is `None`. A literal is read
    /// in pieces so that the bound is on each wait, never on the whole literal.
    pub(crate) async fn read_line_within(
        &mut self,
        stall: Option<Duration>,
    ) -> ImapResult<Vec<u8>> {
        // No cap on the line as a whole: a `FETCH` literal carries a whole message.
        let limits = LineLimits {
            max_literal: MAX_LITERAL,
            max_line: None,
            literal_needs_crlf: false,
        };
        read_framed_line(&mut self.inner, stall, limits).await
    }
}

/// How much one logical line may hold: the largest literal it may announce, and, where there is
/// one, the most bytes in all, literals included.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineLimits {
    pub(crate) max_literal: usize,
    pub(crate) max_line: Option<usize>,
    /// Whether only `{n}` and CRLF announce a literal (ManageSieve, whose tokenizer requires it),
    /// or a bare LF after it does too (IMAP, as it always has).
    pub(crate) literal_needs_crlf: bool,
}

/// Reads one logical line off `reader`: bytes through the next `\n`, with every `{n}` or `{n+}`
/// literal the line announces inlined (the n bytes, then the rest of the line). IMAP and
/// ManageSieve (RFC 5804 §4) frame literals the same way, so both sessions read through this.
///
/// Each wait is bounded by `stall` when there is one, and a literal is read in pieces so that the
/// bound is on each wait, never on the whole literal. A literal over `limits.max_literal` bytes is
/// refused before anything is allocated for it, and where `limits.max_line` is set, so is a line
/// that would grow past it, before the bytes that would take it there are kept.
pub(crate) async fn read_framed_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    stall: Option<Duration>,
    limits: LineLimits,
) -> ImapResult<Vec<u8>> {
    let over = |len: usize| limits.max_line.is_some_and(|max| len > max);
    let mut line = Vec::new();
    loop {
        let before = line.len();
        let read = read_until_newline(reader, &mut line, stall, limits.max_line).await?;
        if read == 0 {
            return Err(closed_mid_response());
        }
        if let Some(len) = trailing_literal_len(&line[before..], limits.literal_needs_crlf) {
            if len > limits.max_literal || over(line.len().saturating_add(len)) {
                return Err(ImapError::protocol(format!(
                    "server announced a {len}-byte literal exceeding the {}-byte cap",
                    limits.max_literal
                )));
            }
            let start = line.len();
            line.resize(start + len, 0);
            let mut filled = start;
            while filled < line.len() {
                let read = within(stall, reader.read(&mut line[filled..])).await?;
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

/// Appends to `line` through the next `\n`, answering how many bytes it appended (0 at the end of
/// the stream). With no `max`, as `read_until` does; with one, refusing to let `line` grow past it.
async fn read_until_newline<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    stall: Option<Duration>,
    max: Option<usize>,
) -> ImapResult<usize> {
    let Some(max) = max else {
        return Ok(within(stall, reader.read_until(b'\n', line)).await?);
    };
    let mut read = 0;
    loop {
        let available = within(stall, reader.fill_buf()).await?;
        if available.is_empty() {
            return Ok(read);
        }
        let (take, done) = match available.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(ImapError::protocol(format!(
                "server sent a line exceeding the {max}-byte cap"
            )));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        read += take;
        if done {
            return Ok(read);
        }
    }
}

/// Awaits one read, bounded by `stall` when there is one.
async fn within<T>(
    stall: Option<Duration>,
    read: impl Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    match stall {
        Some(limit) => deadline::within(limit, "the server sent nothing", read).await,
        None => read.await,
    }
}

fn closed_mid_response() -> ImapError {
    ImapError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "connection closed mid-response",
    ))
}

/// The literal length a line announces (`…{n}` or `…{n+}` before its line end), if any. With
/// `needs_crlf`, only a CRLF ends the announcement, as RFC 5804 §4 writes it, so the reader and
/// ManageSieve's tokenizer (`crate::managesieve::response`) agree on what is a literal; IMAP takes
/// a bare LF too, as it always has.
fn trailing_literal_len(line: &[u8], needs_crlf: bool) -> Option<usize> {
    let trimmed = if needs_crlf {
        line.strip_suffix(b"\r\n")?
    } else {
        let trimmed = line.strip_suffix(b"\n")?;
        trimmed.strip_suffix(b"\r").unwrap_or(trimmed)
    };
    let inside = trimmed.strip_suffix(b"}")?;
    let inside = inside.strip_suffix(b"+").unwrap_or(inside);
    let open = inside.iter().rposition(|&b| b == b'{')?;
    let digits = &inside[open + 1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}
