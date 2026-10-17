//! A folder list with everything the session can have ride on the same `LIST`: roles
//! (RFC 6154), unread counts (RFC 5819) and the caller's rights (RFC 8440).
//!
//! Each is a separate question in base IMAP — one `STATUS`, one `MYRIGHTS` per folder — and
//! each extension exists to fold its question into the one `LIST` a folder sync makes anyway.
//! What the session lacks, [`crate::folders`] asks for separately.

use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    capability::Extension,
    error::ImapResult,
    parse::ListRow,
    transport::Connection,
    transport_command::{ListReturn, list_command},
    unseen::parse_status_unseen,
};

/// One `LIST` answer. The per-folder maps are keyed by the mailbox name **as the server
/// wrote it** — [`ListRow::name`], modified UTF-7 on IMAP4rev1 — which is how a row is paired
/// with its `STATUS` and `MYRIGHTS` lines.
#[derive(Debug)]
pub(crate) struct Listing {
    pub(crate) rows: Vec<ListRow>,
    /// Unread counts, or `None` when the session has no `LIST-STATUS` to ask with. Sparse:
    /// a server sends no `STATUS` for a `\Noselect` container, which holds nothing to count.
    pub(crate) unseen: Option<HashMap<String, u32>>,
    /// Rights letters, or `None` when the session has no `LIST-MYRIGHTS` to ask with.
    /// Sparse: a server that cannot look a folder's rights up sends no `MYRIGHTS` for it
    /// (RFC 8440 §3).
    pub(crate) rights: Option<HashMap<String, String>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `LIST "" <pattern> RETURN (…)` asking for every per-folder datum this session's
    /// extensions allow, in one round trip.
    ///
    /// The roles are asked for alongside the rest because an **extended** `LIST` returns
    /// exactly what its options name (RFC 5258 §3): asking only for the counts is how a
    /// folder list ends up with every badge and no roles, on the same server whose plain
    /// `LIST` volunteers them.
    pub(crate) async fn list_folders(&mut self, pattern: &str) -> ImapResult<Listing> {
        let returning = ListReturn {
            special_use: self.negotiated.must_request_special_use(),
            unseen: self.negotiated.has(Extension::ListStatus),
            myrights: self.negotiated.has(Extension::ListMyrights),
        };
        let response = self
            .command(&list_command(&self.quoted_name(pattern), returning))
            .await?;
        // Untagged only: none of this data rides the completion line, and reading it as
        // though it might invents a mailbox out of the server's prose.
        let lines = response.untagged();
        Ok(Listing {
            rows: crate::parse::parse_list(lines)?,
            unseen: returning.unseen.then(|| parse_status_unseen(lines)),
            rights: returning.myrights.then(|| {
                lines
                    .iter()
                    .filter_map(|line| crate::acl::parse_myrights(line))
                    .collect()
            }),
        })
    }
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
