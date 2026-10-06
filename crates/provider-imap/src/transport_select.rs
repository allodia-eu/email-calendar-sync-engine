//! Opening a mailbox on a session, and remembering which one is open.
//!
//! IMAP has one selected mailbox per connection, and a read addressed by UID needs only two
//! facts about it: that it is the mailbox the key names, and that its `UIDVALIDITY` still
//! matches the key's. A session that already has that mailbox open knows both, so a body read
//! there can skip the `EXAMINE`, which halves the round trips of a warm that reads a folder's
//! bodies one after another. A message's UID must not change during a session (RFC 9051
//! §2.3.1.1), so what the selection read holds for as long as the selection does; a message
//! expunged meanwhile answers its `UID FETCH` with no data, which the read already treats as
//! stale.
//!
//! Split from [`crate::transport`], which owns the line protocol, because this is the only
//! state a command leaves behind on a connection.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    parse::{self, SelectData},
    transport::Connection,
};

/// The mailbox a session has open, as its last successful `SELECT` or `EXAMINE` left it.
///
/// Either verb serves a read: `EXAMINE` is only `SELECT` without write intent. A mutation
/// always selects again, so nothing here needs to remember which of the two it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The mailbox name as the caller passed it, before any wire encoding.
    mailbox: String,
    uid_validity: u32,
    /// `UIDNEXT` as the open reported it, the extent of a search that has to stay under a
    /// message limit ([`crate::transport_search`]).
    pub(crate) uid_next: Option<u32>,
}

impl<S> Connection<S> {
    /// The `UIDVALIDITY` of `mailbox` if this session has it open, else `None`.
    pub(crate) fn open_validity(&self, mailbox: &str) -> Option<u32> {
        self.selected
            .as_ref()
            .filter(|selection| selection.mailbox == mailbox)
            .map(|selection| selection.uid_validity)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `SELECT mailbox`, returning its UID space and message count. Response codes
    /// in either an untagged `* OK [..]` or the tagged completion are honored.
    pub(crate) async fn select(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let command = format!("SELECT {}", self.quoted_name(mailbox));
        self.open_mailbox(&command, mailbox).await
    }

    /// `SELECT mailbox (CONDSTORE)` — opens the mailbox CONDSTORE-aware (RFC 7162
    /// §3.1.8) so the response carries `[HIGHESTMODSEQ n]`, the baseline a QRESYNC
    /// delta records in its cursor. Used in place of [`Connection::select`] for the
    /// sync path on a QRESYNC session.
    pub(crate) async fn select_condstore(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let command = format!("SELECT {} (CONDSTORE)", self.quoted_name(mailbox));
        self.open_mailbox(&command, mailbox).await
    }

    /// `EXAMINE mailbox` — the read-only `SELECT` (RFC 9051 §6.3.2): same response
    /// shape, but opens the mailbox without write intent and does not reset
    /// `\Recent`, so a body peek needs no write access to the folder.
    pub(crate) async fn examine(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let command = format!("EXAMINE {}", self.quoted_name(mailbox));
        self.open_mailbox(&command, mailbox).await
    }

    async fn open_mailbox(&mut self, command: &str, mailbox: &str) -> ImapResult<SelectData> {
        // Forgotten before the command is sent, not after it fails: a `SELECT` deselects
        // whatever was open before it tries (RFC 9051 §6.3.1), so a refused one leaves nothing
        // open, and one that never completed leaves the session in a state nobody can name.
        self.selected = None;
        let response = self.command(command).await?;
        let data = parse::parse_select(&response.into_all_lines())?;
        self.selected = Some(Selection {
            mailbox: mailbox.to_owned(),
            uid_validity: data.uid_validity,
            uid_next: data.uid_next,
        });
        Ok(data)
    }
}
