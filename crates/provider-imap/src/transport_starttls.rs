//! The plaintext half of an IMAP `STARTTLS` upgrade (RFC 9051 §6.2.1).
//!
//! Split from `transport` (which is already near the file-size limit) but operating on
//! the same [`Connection`]: the STARTTLS command runs over the *plaintext* connection
//! reusing the tagged line protocol, then the caller unwraps the raw stream and TLS-
//! wraps it. The post-upgrade session is a normal [`Connection`] built with
//! [`Connection::resume`] — after the upgrade a STARTTLS dial is byte-for-byte an
//! implicit-TLS one, which is why the provider stays generic over one stream type
//! (`docs/agent-guidance/imap-smtp.md`).

use tokio::io::{AsyncRead, AsyncWrite, BufReader};

use crate::{
    error::{ImapError, ImapResult},
    transport::Connection,
};

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// Wraps an already-established stream **without** reading a greeting — the
    /// post-`STARTTLS` resume. After a `STARTTLS` upgrade the server sends no fresh
    /// greeting (the next exchange is `LOGIN`): the plaintext connection consumed the
    /// one greeting before the upgrade, so this resumes login/sync on the TLS stream.
    /// Tags restart at 1; the plaintext connection was consumed by the upgrade.
    pub(crate) fn resume(stream: S) -> Self {
        Self {
            inner: BufReader::new(stream),
            tag: 0,
            qresync: false,
            idle_advertised: false,
            list_status_advertised: false,
            pending_tag: None,
        }
    }

    /// Runs the plaintext `STARTTLS` handshake: confirms the server advertises
    /// `STARTTLS` in its `CAPABILITY`, then issues `STARTTLS` and awaits the tagged
    /// `OK`. The caller upgrades the socket to TLS immediately after (via
    /// [`Connection::into_inner_stream`]).
    ///
    /// Refusing when `STARTTLS` is not advertised is deliberate: it stops the dial
    /// before `LOGIN`, so credentials never cross a link that cannot be upgraded (no
    /// silent cleartext downgrade).
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] if the server does not advertise `STARTTLS`, or the
    /// classified failure of the `CAPABILITY`/`STARTTLS` command.
    pub(crate) async fn start_tls(&mut self) -> ImapResult<()> {
        let response = self.command("CAPABILITY").await?;
        let capabilities = crate::parse_qresync::parse_capabilities(&response.into_all_lines());
        if !capabilities
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case("STARTTLS"))
        {
            return Err(ImapError::protocol(
                "server does not advertise STARTTLS; refusing to send credentials in the clear",
            ));
        }
        self.command("STARTTLS").await?;
        Ok(())
    }

    /// Unwraps the underlying stream after `STARTTLS`, so the caller can TLS-wrap it.
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] if the read buffer holds any bytes past the `STARTTLS`
    /// tagged response. A conformant server sends nothing between that response and
    /// the client-initiated TLS handshake, so buffered plaintext is a command-
    /// injection attempt (the STARTTLS-stripping class, CVE-2011-0411) and MUST NOT be
    /// carried across the TLS boundary — the data is dropped with the connection.
    pub(crate) fn into_inner_stream(self) -> ImapResult<S> {
        if !self.inner.buffer().is_empty() {
            return Err(ImapError::protocol(
                "unexpected buffered data after STARTTLS (possible command injection)",
            ));
        }
        Ok(self.inner.into_inner())
    }
}

#[cfg(test)]
#[path = "transport_starttls_tests.rs"]
mod tests;
