//! Framing for SMTP submission: reading a reply off the wire and writing a command or the
//! message onto it.
//!
//! Split from [`crate::smtp`], which owns the conversation, because this is the part that
//! decides how long to wait for the server ([`crate::deadline`]).

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};

use crate::{
    deadline::{self, BOUNDS},
    error::{ImapError, ImapResult},
};

/// A line-based SMTP stream with multiline-reply assembly, driven by the conversation in
/// [`crate::smtp`] and by the `AUTH` exchange in [`crate::smtp_auth`].
pub(crate) struct SmtpStream<S> {
    inner: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> SmtpStream<S> {
    pub(crate) fn new(stream: S) -> Self {
        Self {
            inner: BufReader::new(stream),
        }
    }

    /// Unwraps the underlying stream after `STARTTLS`, for the TLS upgrade.
    ///
    /// Errors if the read buffer holds any bytes past the `STARTTLS` `220`: a
    /// conformant server sends nothing before the client-initiated TLS handshake, so
    /// buffered plaintext is a command-injection attempt (CVE-2011-0411 class) and
    /// MUST NOT be carried across the TLS boundary.
    pub(crate) fn into_inner_stream(self) -> ImapResult<S> {
        if !self.inner.buffer().is_empty() {
            return Err(ImapError::protocol(
                "unexpected buffered data after STARTTLS (possible command injection)",
            ));
        }
        Ok(self.inner.into_inner())
    }

    /// Reads a (possibly multiline) reply, returning its code and joined text — for
    /// every reply whose content is prose (a greeting, an acceptance, a rejection).
    /// [`read_reply_lines`](Self::read_reply_lines) is the form to use when the content
    /// is a *list* (`EHLO`'s extensions).
    pub(crate) async fn read_reply(&mut self) -> ImapResult<(u16, String)> {
        let (code, lines) = self.read_reply_lines().await?;
        Ok((code, lines.join(" ")))
    }

    /// Reads a (possibly multiline) reply, returning its code and one string per line
    /// (each stripped of its `NNN`/`NNN-` prefix). The continuation-line count is capped
    /// so a server emitting an endless stream of `NNN-...` lines cannot hang the
    /// submission or grow the reply without bound, and each line is awaited for at most
    /// [`Deadlines::reply`](engine_provider::Deadlines::reply), so neither can a server that
    /// stops answering.
    pub(crate) async fn read_reply_lines(&mut self) -> ImapResult<(u16, Vec<String>)> {
        self.read_reply_lines_within(BOUNDS.reply()).await
    }

    /// [`read_reply_lines`](Self::read_reply_lines), awaiting each line for `stall`.
    pub(crate) async fn read_reply_lines_within(
        &mut self,
        stall: Duration,
    ) -> ImapResult<(u16, Vec<String>)> {
        const MAX_REPLY_LINES: usize = 256;
        let mut lines = Vec::new();
        for _ in 0..MAX_REPLY_LINES {
            let mut line = String::new();
            let reading = self.inner.read_line(&mut line);
            if deadline::within(stall, "the server sent nothing", reading).await? == 0 {
                return Err(ImapError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "SMTP connection closed",
                )));
            }
            let trimmed = line.trim_end();
            let code: u16 = trimmed
                .get(0..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| ImapError::protocol(format!("malformed SMTP reply: {trimmed}")))?;
            lines.push(trimmed.get(4..).unwrap_or("").to_owned());
            if trimmed.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, lines));
            }
        }
        Err(ImapError::protocol(
            "SMTP multiline reply exceeded the line cap",
        ))
    }

    pub(crate) async fn write_line(&mut self, line: &str) -> ImapResult<()> {
        self.write(format!("{line}\r\n").as_bytes()).await
    }

    /// Writes the message body dot-stuffed, then the `<CRLF>.<CRLF>` terminator.
    pub(crate) async fn write_data(&mut self, message: &[u8]) -> ImapResult<()> {
        let mut data = dot_stuff(message);
        data.extend_from_slice(b".\r\n");
        self.write(&data).await
    }

    /// Writes `bytes` and flushes, a piece at a time ([`deadline::write`]).
    async fn write(&mut self, bytes: &[u8]) -> ImapResult<()> {
        Ok(deadline::write(&mut self.inner, bytes).await?)
    }
}

/// Dot-stuffs a CRLF-delimited message: any line beginning with `.` gets a second
/// leading `.` so it is not mistaken for the terminator (RFC 5321 §4.5.2).
fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut start = 0;
    while start < message.len() {
        let end = message[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(message.len(), |p| start + p + 1);
        let line = &message[start..end];
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::dot_stuff;

    #[test]
    fn dot_stuffing_escapes_leading_dots() {
        let stuffed = dot_stuff(b".hidden\r\nnormal\r\n..already\r\n");
        let text = String::from_utf8(stuffed).unwrap();
        // A line beginning with `.` gets a second `.`; others are untouched.
        assert!(text.starts_with("..hidden\r\n"));
        assert!(text.contains("\r\nnormal\r\n"));
        assert!(text.contains("\r\n...already\r\n"));
    }
}
