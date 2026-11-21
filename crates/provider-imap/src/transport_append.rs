//! The two commands that **place a message in a mailbox**: `CREATE` and `APPEND`.
//!
//! Split from `transport` (which is at the file-size limit) but operating on the same
//! [`Connection`], exactly as the STARTTLS half is. They are the write side of the
//! transport — everything else it speaks reads or reconciles — and they are what
//! [`crate::place`] drives to file a sent copy or save a draft, and what an import
//! (`crate::import`) stores a message with.

use engine_core::time::UtcDateTime;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    transport::{Connection, parse_append_uid, strip_ascii_prefix},
};

/// Why an `APPEND` did not store its message, and whether it may have anyway.
#[derive(Debug)]
pub(crate) enum AppendFailure {
    /// The message was not stored: the server refused it (a tagged `NO` or `BAD`), or the
    /// connection failed before the whole command reached the server.
    NotStored(ImapError),
    /// The whole command went out and no answer came back, so the server may have stored the
    /// message. `APPEND` is not idempotent: sending it again may store it twice.
    Unanswered(ImapError),
}

impl AppendFailure {
    /// The underlying error, for a caller that does not tell the two apart.
    pub(crate) fn into_error(self) -> ImapError {
        match self {
            Self::NotStored(err) | Self::Unanswered(err) => err,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `CREATE <mailbox>`. Used to ensure the Sent folder exists before filing a
    /// copy; an "already exists" rejection is the caller's to ignore.
    pub(crate) async fn create(&mut self, mailbox: &str) -> ImapResult<()> {
        self.command(&format!("CREATE {}", self.quoted_name(mailbox)))
            .await?;
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
        self.append_dated(mailbox, flags, None, message)
            .await
            .map_err(AppendFailure::into_error)
    }

    /// [`append`](Self::append), stamping the stored message's `INTERNALDATE` with
    /// `received_at` where given (the server stamps the time of the `APPEND` otherwise), and
    /// saying whether a failure may have stored the message ([`AppendFailure`]).
    pub(crate) async fn append_dated(
        &mut self,
        mailbox: &str,
        flags: &str,
        received_at: Option<UtcDateTime>,
        message: &[u8],
    ) -> Result<Option<(u32, u32)>, AppendFailure> {
        let tag = self.next_tag();
        let date = received_at.map_or_else(String::new, |at| format!(" \"{}\"", internal_date(at)));
        // A synchronizing literal: send the header, await the `+` continuation, then
        // the raw bytes.
        let header = format!(
            "{tag} APPEND {} ({flags}){date} {{{}}}\r\n",
            self.quoted_name(mailbox),
            message.len()
        );
        self.send_raw(header.as_bytes())
            .await
            .map_err(AppendFailure::NotStored)?;
        self.await_continuation(&tag)
            .await
            .map_err(AppendFailure::NotStored)?;
        // The literal goes a piece at a time, each under the stall bound, so a large message
        // moves however slowly it must and a server that stops reading cannot hold it. Until
        // the closing CRLF is written the command is incomplete, so the server stores nothing.
        self.send_raw(message)
            .await
            .map_err(AppendFailure::NotStored)?;
        self.send_raw(b"\r\n")
            .await
            .map_err(AppendFailure::NotStored)?;
        match self.read_response(&tag).await {
            Ok(response) => Ok(parse_append_uid(&response.detail)),
            Err(refused @ (ImapError::No(_) | ImapError::Bad(_))) => {
                Err(AppendFailure::NotStored(refused))
            }
            Err(lost) => Err(AppendFailure::Unanswered(lost)),
        }
    }

    /// Waits for the `+` continuation an `APPEND`'s literal needs. The server may emit untagged
    /// responses (e.g. `* n EXISTS`) first, which are skipped (RFC 9051 §7 allows unsolicited
    /// untagged responses at any point), and it may refuse the command instead, answering the
    /// tagged `NO` or `BAD` before the literal is sent (RFC 7889's `TOOBIG`).
    async fn await_continuation(&mut self, tag: &str) -> ImapResult<()> {
        let tagged = format!("{tag} ");
        loop {
            let line = self.read_line().await?;
            if strip_ascii_prefix(&line, b"* ").is_some() {
                continue;
            }
            if strip_ascii_prefix(&line, b"+").is_some() {
                return Ok(());
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end();
            if let Some(completion) = text.strip_prefix(&tagged) {
                if let Some(detail) = completion.strip_prefix("NO ") {
                    return Err(ImapError::no(detail));
                }
                if let Some(detail) = completion.strip_prefix("BAD ") {
                    return Err(ImapError::bad(detail));
                }
            }
            return Err(ImapError::protocol(format!(
                "APPEND expected a continuation, got: {text}"
            )));
        }
    }
}

/// `at` as an IMAP `date-time` (RFC 9051 §9): `dd-Mon-yyyy hh:mm:ss +0000`, in UTC.
fn internal_date(at: UtcDateTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[usize::from(at.month().clamp(1, 12)) - 1];
    format!(
        "{:02}-{month}-{:04} {:02}:{:02}:{:02} +0000",
        at.day(),
        at.year(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

#[cfg(test)]
mod tests {
    use super::internal_date;

    #[test]
    fn an_internal_date_is_the_instant_in_utc_with_a_two_digit_day() {
        let at = "2019-02-03T04:05:06Z".parse().unwrap();
        assert_eq!(internal_date(at), "03-Feb-2019 04:05:06 +0000");
        let at = "2024-12-31T23:59:59Z".parse().unwrap();
        assert_eq!(internal_date(at), "31-Dec-2024 23:59:59 +0000");
    }
}
