//! The IMAP command vocabulary: one method per command the engine speaks.
//!
//! A second `impl Connection<S>` block, split from `transport` (which owns the tagged
//! framing: the greeting, literals, `command`/`read_response`, login, and capability
//! negotiation) so each file stays under the 500-line limit. Every method here is a thin
//! "format the command, hand the untagged lines to a parser" pair — the parsing itself is
//! pure and lives in `crate::parse` and its siblings.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::{
    acl::{self, MailboxRights},
    error::{ImapError, ImapResult},
    namespace::{self, Namespaces},
    parse::{self, FetchRow, ListRow, SelectData},
    transport::{Connection, parse_append_uid, quote, strip_ascii_prefix},
};

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `SELECT mailbox`, returning its UID space and message count. Response codes in
    /// either an untagged `* OK [..]` or the tagged completion are honored.
    pub(crate) async fn select(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self.command(&format!("SELECT {}", quote(mailbox))).await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `SELECT mailbox (CONDSTORE)` — opens the mailbox CONDSTORE-aware (RFC 7162
    /// §3.1.8) so the response carries `[HIGHESTMODSEQ n]`, the baseline a QRESYNC
    /// delta records in its cursor. Used in place of [`Connection::select`] for the
    /// sync path on a QRESYNC session.
    pub(crate) async fn select_condstore(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self
            .command(&format!("SELECT {} (CONDSTORE)", quote(mailbox)))
            .await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `EXAMINE mailbox` — the read-only `SELECT` (RFC 9051 §6.3.2): same response
    /// shape, but opens the mailbox without write intent and does not reset
    /// `\Recent`, so a body peek needs no write access to the folder.
    pub(crate) async fn examine(&mut self, mailbox: &str) -> ImapResult<SelectData> {
        let response = self.command(&format!("EXAMINE {}", quote(mailbox))).await?;
        parse::parse_select(&response.into_all_lines())
    }

    /// `UID FETCH <set> (<items>)`, returning the parsed rows.
    pub(crate) async fn uid_fetch(&mut self, set: &str, items: &str) -> ImapResult<Vec<FetchRow>> {
        let response = self.command(&format!("UID FETCH {set} ({items})")).await?;
        parse::parse_fetch(&response.untagged)
    }

    /// `UID FETCH <set> (<items>) (CHANGEDSINCE <modseq> VANISHED)` — the QRESYNC
    /// incremental delta (RFC 7162 §3.1.4.1, §3.2.5). The server returns a `FETCH` for
    /// every message whose mod-sequence is greater than `modseq` (new arrivals *and*
    /// flag changes, with full metadata) and a `* VANISHED (EARLIER) <set>` listing the
    /// UIDs expunged since `modseq`. Returns the changed rows paired with the expanded
    /// vanished UIDs, both read from the one command's untagged responses.
    pub(crate) async fn uid_fetch_changedsince(
        &mut self,
        set: &str,
        items: &str,
        modseq: u64,
    ) -> ImapResult<(Vec<FetchRow>, Vec<u32>)> {
        let response = self
            .command(&format!(
                "UID FETCH {set} ({items}) (CHANGEDSINCE {modseq} VANISHED)"
            ))
            .await?;
        let rows = parse::parse_fetch(&response.untagged)?;
        let vanished = crate::parse_qresync::parse_vanished(&response.untagged);
        Ok((rows, vanished))
    }

    /// `UID SEARCH SINCE <date>` — the UIDs of messages whose `INTERNALDATE` is on or
    /// after `date` (an IMAP `dd-Mon-yyyy` date, RFC 9051 §6.4.4), used to find the
    /// floor of a sync-depth window so a snapshot fetches only recent mail. `date` is
    /// caller-formatted from a calendar date (digits + a fixed month abbreviation), so
    /// it carries no quoting or injection risk. Returns the matched UIDs (empty if none
    /// match), tolerating both the classic `* SEARCH` and extended `* ESEARCH` reply.
    pub(crate) async fn uid_search_since(&mut self, date: &str) -> ImapResult<Vec<u32>> {
        let response = self.command(&format!("UID SEARCH SINCE {date}")).await?;
        Ok(parse::parse_search(&response.untagged))
    }

    /// `UID FETCH <uid> (BODY.PEEK[])`, returning the raw RFC 5322 bytes of the
    /// message (the whole source, headers + every part), or `None` if the server
    /// returned no `BODY[]` for that UID — i.e. it was expunged since the last sync
    /// (fetching a non-existent UID is a tagged `OK` with no data, RFC 9051 §6.4.8).
    /// `.PEEK` does not set `\Seen` — fetching a body to read it must not silently
    /// mark it read; the host decides that via a separate edit. Only the matching
    /// UID's data is accepted, so an unsolicited `FETCH` for another UID (a
    /// concurrent flag update) cannot return the wrong message's bytes.
    pub(crate) async fn uid_fetch_body(&mut self, uid: u32) -> ImapResult<Option<Vec<u8>>> {
        let response = self
            .command(&format!("UID FETCH {uid} (BODY.PEEK[])"))
            .await?;
        Ok(parse::parse_fetch_body(&response.untagged, uid))
    }

    /// `LIST "" <pattern>`, returning the matching mailboxes.
    ///
    /// `pattern` is an IMAP list-mailbox wildcard string (`*` any depth, `%` one level) and
    /// is sent quoted, so a prefix containing a space — `"Shared Folders/support@…/*"`,
    /// exactly the shape a shared namespace produces — is one argument rather than two.
    ///
    /// There is no unscoped `list()` on purpose: `"*"` returns the credential's own folders
    /// *and* every folder shared with it, so every caller has to say which store it means
    /// (`crate::discovery`).
    pub(crate) async fn list_pattern(&mut self, pattern: &str) -> ImapResult<Vec<ListRow>> {
        let response = self
            .command(&format!(r#"LIST "" {}"#, quote(pattern)))
            .await?;
        parse::parse_list(&response.untagged)
    }

    /// `NAMESPACE` (RFC 2342), returning the personal / other-users' / shared prefixes.
    ///
    /// The one command that answers *whose* mail a folder holds. Only issued when the
    /// server advertised `NAMESPACE` post-auth; a server without it reports nothing and
    /// every mailbox is read as the credential's own.
    pub(crate) async fn namespace(&mut self) -> ImapResult<Namespaces> {
        let response = self.command("NAMESPACE").await?;
        Ok(namespace::parse_namespace(&response.untagged))
    }

    /// `MYRIGHTS <mailbox>` (RFC 4314 §3.8), returning the caller's rights on it.
    ///
    /// `Ok(None)` means *unknown*, not *no rights*, and covers two real cases: a server
    /// without the ACL extension, and a mailbox the command does not apply to. The second
    /// is not hypothetical — Stalwart answers `NO Mailbox does not exist.` for the
    /// `\NoSelect` container a shared namespace introduces (`Shared Folders`), which is a
    /// path component rather than a mailbox. Neither is a reason to fail a folder sync, so
    /// a `NO`/`BAD` is folded into `None` while a transport error still propagates.
    pub(crate) async fn myrights(&mut self, mailbox: &str) -> ImapResult<Option<MailboxRights>> {
        match self.command(&format!("MYRIGHTS {}", quote(mailbox))).await {
            Ok(response) => Ok(acl::parse_myrights(&response.untagged)),
            Err(ImapError::No(_) | ImapError::Bad(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

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

    /// `UID STORE <set> <item>` — alters the flags of the named UIDs, where `item`
    /// is e.g. `+FLAGS.SILENT (\Seen)` or `-FLAGS.SILENT (\Flagged)` (RFC 9051
    /// §6.4.6). The `.SILENT` suffix suppresses the per-message `FETCH` echo, so no
    /// response parsing is needed — a tagged `OK` is success, a `NO`/`BAD` an error.
    pub(crate) async fn uid_store(&mut self, set: &str, item: &str) -> ImapResult<()> {
        self.command(&format!("UID STORE {set} {item}")).await?;
        Ok(())
    }

    /// `UID MOVE <set> <mailbox>` — moves the named UIDs to `dest` (RFC 6851), so
    /// the move is atomic server-side (copy + `\Deleted` + expunge in one command,
    /// where supported). The destination is a quoted string.
    pub(crate) async fn uid_move(&mut self, set: &str, dest: &str) -> ImapResult<()> {
        self.command(&format!("UID MOVE {set} {}", quote(dest)))
            .await?;
        Ok(())
    }

    /// `UID EXPUNGE <set>` — permanently removes only the named `\Deleted` UIDs
    /// (UIDPLUS, RFC 4315), so a concurrent `\Deleted` mark elsewhere in the mailbox
    /// is not collaterally expunged.
    pub(crate) async fn uid_expunge(&mut self, set: &str) -> ImapResult<()> {
        self.command(&format!("UID EXPUNGE {set}")).await?;
        Ok(())
    }
}
