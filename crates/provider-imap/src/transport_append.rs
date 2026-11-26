//! The two commands that **place a message in a mailbox**: `CREATE` and `APPEND`.
//!
//! Split from `transport` (which is at the file-size limit) but operating on the same
//! [`Connection`], exactly as the STARTTLS half is. They are the write side of the
//! transport — everything else it speaks reads or reconciles — and they are what
//! [`crate::store_message`] stores every message with: a sent copy, a draft, an import.

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
    /// `CREATE <mailbox>`, or `CREATE <mailbox> (USE (<attribute>))` to give it the
    /// special-use `attribute` too (RFC 6154 §3), which only a server advertising
    /// `CREATE-SPECIAL-USE` may be sent. An "already exists" rejection is the caller's to judge.
    pub(crate) async fn create(
        &mut self,
        mailbox: &str,
        special_use: Option<&str>,
    ) -> ImapResult<()> {
        let with_use = special_use
            .map(|attribute| format!(" (USE ({attribute}))"))
            .unwrap_or_default();
        let command = format!("CREATE {}{with_use}", self.quoted_name(mailbox));
        self.command(&command).await?;
        Ok(())
    }

    /// `APPEND <mailbox> (<flags>) ["<date-time>"] {N}` followed by the message literal,
    /// stamping the stored message's `INTERNALDATE` with `received_at` where given (the server
    /// stamps the time of the `APPEND` otherwise).
    ///
    /// Returns the `[APPENDUID validity uid]` when the server supports UIDPLUS, so the caller
    /// can key the object; `None` otherwise (it then reconciles by `Message-ID` on a later
    /// sync). A failure says whether the message may have been stored ([`AppendFailure`]).
    pub(crate) async fn append(
        &mut self,
        mailbox: &str,
        flags: &str,
        received_at: Option<UtcDateTime>,
        message: &[u8],
    ) -> Result<Option<(u32, u32)>, AppendFailure> {
        // A streamed `UID FETCH` abandoned on this connection is still answering. Read it to
        // its tag first, as every tagged command does, or a line of a body it streams that
        // opens with `+` reads as this command's continuation.
        self.drain_pending()
            .await
            .map_err(AppendFailure::NotStored)?;
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
                // The status is an atom, so case does not matter, and its text may be empty
                // (RFC 9051 `resp-text`).
                let (status, detail) = completion.split_once(' ').unwrap_or((completion, ""));
                if status.eq_ignore_ascii_case("NO") {
                    return Err(ImapError::no(detail));
                }
                if status.eq_ignore_ascii_case("BAD") {
                    return Err(ImapError::bad(detail));
                }
            }
            return Err(ImapError::protocol(format!(
                "APPEND expected a continuation, got: {text}"
            )));
        }
    }
}

/// The instant an `INTERNALDATE` stores for `at`: whole seconds, since a `date-time` holds no
/// fraction, or `None` where `at` falls outside the four-digit `date-year` (RFC 9051 §9).
pub(crate) fn internal_date_instant(at: UtcDateTime) -> Option<UtcDateTime> {
    if at.year() < 0 {
        return None;
    }
    UtcDateTime::new(
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
    .ok()
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
    use super::{AppendFailure, internal_date};
    use crate::{
        error::ImapError,
        mock::{MockStream, script, written},
        transport::Connection,
    };

    #[test]
    fn an_internal_date_is_the_instant_in_utc_with_a_two_digit_day() {
        let at = "2019-02-03T04:05:06Z".parse().unwrap();
        assert_eq!(internal_date(at), "03-Feb-2019 04:05:06 +0000");
        let at = "2024-12-31T23:59:59Z".parse().unwrap();
        assert_eq!(internal_date(at), "31-Dec-2024 23:59:59 +0000");
    }

    #[test]
    fn an_internal_date_holds_whole_seconds_in_a_four_digit_year() {
        use engine_core::time::UtcDateTime;

        use super::internal_date_instant;

        let at = "2019-02-03T04:05:06.5Z".parse::<UtcDateTime>().unwrap();
        assert_eq!(
            internal_date_instant(at),
            Some("2019-02-03T04:05:06Z".parse().unwrap())
        );
        let before_year_zero = UtcDateTime::new(-1, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(internal_date_instant(before_year_zero), None);
    }

    #[tokio::test]
    async fn an_append_first_drains_a_streamed_fetch_left_unread() {
        // An abandoned streamed `UID FETCH` is still answering. A line of the body it streams
        // may open with `+`, which must not be read as this `APPEND`'s continuation.
        let (stream, recorded) = MockStream::new(script(&[
            "* 1 FETCH (UID 5 BODY[] {9}\r\n",
            "+1 idea\r\n",
            ")\r\n",
            "a9 OK FETCH done\r\n",
            "+ go\r\n",
            "a1 OK [APPENDUID 7 12] APPEND done\r\n",
        ]));
        let mut connection = Connection::resume(stream);
        connection.pending_tag = Some("a9".to_owned());

        let stored = connection.append("INBOX", "", None, b"one").await;

        assert_eq!(stored.unwrap(), Some((7, 12)), "{}", written(&recorded));
    }

    #[tokio::test]
    async fn a_refusal_in_place_of_the_continuation_may_carry_no_text() {
        // RFC 9051 `resp-text` may be empty, and a status word is case-insensitive.
        for refusal in ["a1 NO \r\n", "a1 no [TOOBIG] too big\r\n", "a1 BAD \r\n"] {
            let (stream, _) = MockStream::new(script(&[refusal]));
            let mut connection = Connection::resume(stream);

            let refused = connection.append("INBOX", "", None, b"one").await;

            assert!(
                matches!(
                    refused,
                    Err(AppendFailure::NotStored(
                        ImapError::No(_) | ImapError::Bad(_)
                    ))
                ),
                "{refusal:?}: {refused:?}"
            );
        }
    }
}
