//! Incremental `UID FETCH` streaming on [`Connection`]: pull rows one at a time as
//! they parse off the wire, so the sync layer commits mail sub-batch on a slow server
//! (`store-and-sync.md`) rather than only after the whole batch downloads.
//!
//! Split out of `transport.rs` to keep each file within the size limit; the methods
//! live in their own `impl` block on the same [`Connection`].

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    deadline::BOUNDS,
    error::{ImapError, ImapResult},
    parse::{self, FetchRow},
    transport::{Connection, strip_ascii_prefix},
};

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Drains an abandoned streamed `UID FETCH`: reads and discards untagged lines
    /// until its tagged completion, clearing `pending_tag`. A no-op when no streamed
    /// fetch is in flight. Called by `command` so a fresh command self-heals a
    /// connection whose prior streamed fetch was dropped mid-response.
    pub(crate) async fn drain_pending(&mut self) -> ImapResult<()> {
        let Some(tag) = self.pending_tag.take() else {
            return Ok(());
        };
        let prefix = format!("{tag} ");
        loop {
            let line = self.read_line().await?;
            if strip_ascii_prefix(&line, b"* ").is_some() {
                continue;
            }
            if String::from_utf8_lossy(&line).starts_with(&prefix) {
                return Ok(());
            }
        }
    }

    /// Starts a streamed `UID FETCH set items`: sends the command and records its tag,
    /// so the caller can then pull rows one at a time with [`Self::next_fetch_row`] as
    /// they parse off the wire.
    ///
    /// # Errors
    ///
    /// [`ImapError::Io`] on a transport failure while sending.
    pub(crate) async fn uid_fetch_stream_start(
        &mut self,
        set: &str,
        items: &str,
    ) -> ImapResult<()> {
        self.drain_pending().await?;
        let tag = self.next_tag();
        // The item list must be parenthesized (RFC 9051 `fetch`); an unparenthesized
        // multi-item list makes a lenient server (Stalwart) parse only the first att.
        let request = format!("{tag} UID FETCH {set} ({items})\r\n");
        self.send_raw(request.as_bytes()).await?;
        self.pending_tag = Some(tag);
        Ok(())
    }

    /// Reads the next `FETCH` row of the streamed command started by
    /// [`Self::uid_fetch_stream_start`], or `None` at its tagged completion (clearing
    /// the pending state). Non-`FETCH` untagged responses (e.g. `* n EXISTS`) are
    /// skipped. Calling it with no streamed fetch in flight returns `None`.
    ///
    /// # Errors
    ///
    /// [`ImapError::No`]/[`ImapError::Bad`] if the command completed with a failure,
    /// [`ImapError::Protocol`] on an unexpected line, or [`ImapError::Io`] on transport.
    pub(crate) async fn next_fetch_row(&mut self) -> ImapResult<Option<FetchRow>> {
        let Some(tag) = self.pending_tag.clone() else {
            return Ok(None);
        };
        let prefix = format!("{tag} ");
        loop {
            let line = self.read_line().await?;
            if let Some(body) = strip_ascii_prefix(&line, b"* ") {
                // Parse this one untagged line; a non-FETCH response yields no row.
                if let Some(row) = parse::parse_fetch(&[body.to_vec()])?.into_iter().next() {
                    return Ok(Some(row));
                }
                continue;
            }
            // The tagged completion ends the stream.
            self.pending_tag = None;
            return stream_completion(&line, &prefix).map(|()| None);
        }
    }

    /// Reads the next `BODY[]` of a streamed `UID FETCH <set> (BODY.PEEK[])` started by
    /// [`Self::uid_fetch_stream_start`], as `(uid, bytes)`, or `None` at its tagged
    /// completion. Lines carrying no body (a piggybacked flag update, `* n EXISTS`) are
    /// skipped, and every read is bounded by
    /// [`Deadlines::stall`](engine_provider::Deadlines::stall), as a single body's is.
    ///
    /// # Errors
    ///
    /// As [`Self::next_fetch_row`], and [`ImapError::Io`] of kind `TimedOut` when the server
    /// goes silent.
    pub(crate) async fn next_fetch_body(&mut self) -> ImapResult<Option<(u32, Vec<u8>)>> {
        let Some(tag) = self.pending_tag.clone() else {
            return Ok(None);
        };
        let prefix = format!("{tag} ");
        loop {
            let line = self.read_line_within(Some(BOUNDS.stall())).await?;
            if let Some(body) = strip_ascii_prefix(&line, b"* ") {
                if let Some(row) = crate::parse_body::parse_any_fetch_body(body) {
                    return Ok(Some(row));
                }
                continue;
            }
            self.pending_tag = None;
            return stream_completion(&line, &prefix).map(|()| None);
        }
    }
}

/// Reads the tagged completion that ends a streamed fetch: `OK` is the end, `NO`/`BAD` a
/// failure of the command, anything else a line that does not belong.
fn stream_completion(line: &[u8], prefix: &str) -> ImapResult<()> {
    let text = String::from_utf8_lossy(line);
    let Some(rest) = text.strip_prefix(prefix) else {
        return Err(ImapError::protocol(format!(
            "unexpected line during streamed fetch: {}",
            text.trim()
        )));
    };
    let mut parts = rest.trim_end().splitn(2, ' ');
    let status = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default().to_owned();
    match status.to_ascii_uppercase().as_str() {
        "OK" => Ok(()),
        "NO" => Err(ImapError::no(detail)),
        "BAD" => Err(ImapError::bad(detail)),
        other => Err(ImapError::protocol(format!("unknown completion {other}"))),
    }
}
