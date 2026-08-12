//! The two commands that **place a message in a mailbox**: `CREATE` and `APPEND`.
//!
//! Split from `transport` (which is at the file-size limit) but operating on the same
//! [`Connection`], exactly as the STARTTLS half is. They are the write side of the
//! transport — everything else it speaks reads or reconciles — and they are what
//! [`crate::place`] drives to file a sent copy or save a draft.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{
    error::{ImapError, ImapResult},
    transport::{Connection, parse_append_uid, quote, strip_ascii_prefix},
};

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `CREATE <mailbox>`. Used to ensure the Sent folder exists before filing a
    /// copy; an "already exists" rejection is the caller's to ignore.
    pub(crate) async fn create(&mut self, mailbox: &str) -> ImapResult<()> {
        self.command(&format!("CREATE {}", quote(mailbox))).await?;
        Ok(())
    }

    /// `APPEND <mailbox> (<flags>) {N}` followed by the message literal — used to
    /// file a sent copy in Sent (`\Seen`) or save a draft in Drafts (`\Draft`).
    /// Returns the `[APPENDUID validity uid]` when the server supports UIDPLUS, so
    /// the caller can key the object; `None` otherwise (it then reconciles by
    /// `Message-ID` on a later sync).
    pub(crate) async fn append(
        &mut self,
        mailbox: &str,
        flags: &str,
        message: &[u8],
    ) -> ImapResult<Option<(u32, u32)>> {
        let tag = self.next_tag();
        // A synchronizing literal: send the header, await the `+` continuation, then
        // the raw bytes.
        let header = format!(
            "{tag} APPEND {} ({flags}) {{{}}}\r\n",
            quote(mailbox),
            message.len()
        );
        self.inner.write_all(header.as_bytes()).await?;
        self.inner.flush().await?;
        // The server may emit untagged responses (e.g. `* n EXISTS`) before the `+`
        // continuation request; skip them and wait for the continuation (RFC 9051
        // §7 allows unsolicited untagged responses at any point).
        loop {
            let line = self.read_line().await?;
            if strip_ascii_prefix(&line, b"* ").is_some() {
                continue;
            }
            if strip_ascii_prefix(&line, b"+ ").is_some() {
                break;
            }
            return Err(ImapError::protocol(format!(
                "APPEND expected a continuation, got: {}",
                String::from_utf8_lossy(&line).trim()
            )));
        }
        self.inner.write_all(message).await?;
        self.inner.write_all(b"\r\n").await?;
        self.inner.flush().await?;
        let response = self.read_response(&tag).await?;
        Ok(parse_append_uid(&response.detail))
    }
}
