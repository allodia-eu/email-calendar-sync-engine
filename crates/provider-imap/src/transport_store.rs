//! The two commands that say whose mail a folder is and what may be done in it:
//! `NAMESPACE` (RFC 2342) and `MYRIGHTS` (RFC 4314).
//!
//! Beside the connection rather than inside it, like `crate::unseen`, so the line protocol in
//! `crate::transport` stays under the size limit.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    store::{Namespaces, parse_namespace},
    transport::{Connection, strip_ascii_prefix},
};

/// How many `MYRIGHTS` go out before their answers are read.
///
/// Pipelining is what makes per-folder rights affordable — RFC 4314 has no bulk form, and
/// one round trip per folder is what a folder-list sync must not grow by — but writing
/// every command before reading any answer can stall once both directions' buffers fill.
/// A bounded batch keeps one round trip per sixty-four folders, which is one for any
/// ordinary account.
const RIGHTS_BATCH: usize = 64;

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `NAMESPACE` — which prefixes are the credential's own and which are someone else's,
    /// decoded the way this session's mailbox names are.
    ///
    /// # Errors
    ///
    /// The classified failure of the command. An answer the parser cannot read is not
    /// one: it yields no namespaces, the reading a server without the extension has always
    /// had (`crate::store`).
    pub(crate) async fn namespace(&mut self) -> ImapResult<Namespaces> {
        let response = self.command("NAMESPACE").await?;
        Ok(parse_namespace(
            response.untagged(),
            self.names_are_modified_utf7(),
        ))
    }

    /// `MYRIGHTS` for each of `mailboxes`, pipelined; the answers come back in the order
    /// asked.
    ///
    /// An entry is `None` where the server gave no rights — a `NO` or `BAD` for that one
    /// mailbox, or a completion with no `MYRIGHTS` line. That is "unknown", and it costs only
    /// that entry.
    ///
    /// Answers are paired with questions **by mailbox name**, never by position. RFC 9051
    /// §5.5 lets a server work on the next command before finishing the current one, and
    /// Stalwart does: pipelined `MYRIGHTS` complete out of order, so the tagged completion
    /// after a `* MYRIGHTS` line is not necessarily the command that asked. A batch is read
    /// until every tag it sent has completed, whatever order they arrive in.
    ///
    /// # Errors
    ///
    /// A transport or protocol failure — including a completion for a tag this batch never
    /// sent — after which the session is not usable anyway.
    pub(crate) async fn myrights(
        &mut self,
        mailboxes: &[String],
    ) -> ImapResult<Vec<Option<String>>> {
        // A streamed `UID FETCH` abandoned mid-response would otherwise answer first.
        self.drain_pending().await?;
        let modified_utf7 = self.names_are_modified_utf7();
        let mut answered: HashMap<String, String> = HashMap::new();
        for batch in mailboxes.chunks(RIGHTS_BATCH) {
            let mut commands = String::new();
            let mut pending = HashSet::with_capacity(batch.len());
            for mailbox in batch {
                let tag = self.next_tag();
                let _ = write!(commands, "{tag} MYRIGHTS {}\r\n", self.quoted_name(mailbox));
                pending.insert(tag);
            }
            self.send_raw(commands.as_bytes()).await?;
            while !pending.is_empty() {
                let line = self.read_line().await?;
                if let Some(body) = strip_ascii_prefix(&line, b"* ") {
                    // Any other untagged data (an `EXISTS`, an alert) is not an answer here.
                    if let Some((name, letters)) = crate::acl::parse_myrights(body) {
                        let name = if modified_utf7 {
                            crate::utf7::decode(&name)
                        } else {
                            name
                        };
                        answered.insert(name, letters);
                    }
                    continue;
                }
                // `OK`, `NO` or `BAD` alike close the command; a refusal just leaves its
                // mailbox without an answer.
                let text = String::from_utf8_lossy(&line);
                let tag = text.split(' ').next().unwrap_or_default();
                if !pending.remove(tag) {
                    return Err(ImapError::protocol(format!(
                        "unexpected line while reading MYRIGHTS: {}",
                        text.trim()
                    )));
                }
            }
        }
        Ok(mailboxes
            .iter()
            .map(|mailbox| answered.get(mailbox).cloned())
            .collect())
    }
}

#[cfg(test)]
#[path = "transport_store_tests.rs"]
mod tests;
