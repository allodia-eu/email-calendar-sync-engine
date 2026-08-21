//! Reporting a message as junk / not junk / phishing over IMAP.
//!
//! IMAP has no report command. The report is the IANA-registered keyword — `$Junk`,
//! `$NotJunk`, `$Phishing` ("IMAP and JMAP Keywords" registry, RFC 5788; `$Phishing`
//! per RFC 9979) — set with `UID STORE`, followed by a `UID MOVE` that files the
//! message. Two commands rather than JMAP's one, because IMAP keeps the flag and the
//! folder in different verbs.
//!
//! **The keyword is stored only if the server allows new keywords**, and that is not a
//! given. RFC 9051 §7.1 makes `\*` in the `SELECT` response's `PERMANENTFLAGS` the
//! server's statement that a client may create them; without it a server answers
//! `UID STORE +FLAGS ($Junk)` with a plain `OK` and keeps nothing. The report would
//! read as delivered and be absent on the next `FETCH` — a silent success, and the
//! reason this path checks the response code before it writes rather than trusting the
//! `OK`. Stalwart advertises `\*` (verified live), so the refusal branch is the one
//! the harness cannot exercise.
//!
//! Both keywords in the contradicting pair are handled: the verdict's is added and its
//! opposite removed, so a message is never left asserting it is both junk and not.

use engine_core::ids::ProviderKey;
use engine_provider::{MessageReport, ProviderError, ProviderResult, ReportReceipt, ReportVerdict};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    target::{Access, reject_control_chars, select_target},
    transport::Connection,
};

/// The keyword a verdict sets, and the one it clears.
///
/// Spelled in the registry's canonical mixed case. IMAP keywords are
/// case-insensitive (RFC 9051 §2.3.2), so this is a readability choice, not a
/// correctness one — but it is the spelling a server operator will recognise in a log.
/// "Not junk" clears **both** accusations: the user is vouching for the message, and
/// leaving `$Phishing` set would keep the stronger claim standing against it.
const fn keywords_for(verdict: ReportVerdict) -> (&'static str, &'static [&'static str]) {
    match verdict {
        ReportVerdict::Junk => ("$Junk", &["$NotJunk"]),
        ReportVerdict::NotJunk => ("$NotJunk", &["$Junk", "$Phishing"]),
        ReportVerdict::Phishing => ("$Phishing", &["$NotJunk"]),
    }
}

/// Reports `report.target` over `connection`: keyword `STORE`, then `MOVE` into the
/// destination.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if the target key is unparseable, a mailbox name carries a
///   control character, or the target mailbox does not permit new keywords (no `\*` in
///   `PERMANENTFLAGS`) — the report is refused rather than written into a flag the server will
///   discard.
/// - [`ProviderError::conflict`] if the target mailbox's `UIDVALIDITY` has moved (the key is stale;
///   re-sync, then retry).
/// - A classified [`ProviderError`] from the underlying command.
pub(crate) async fn report_message<S>(
    connection: &mut Connection<S>,
    report: &MessageReport,
) -> ProviderResult<ReportReceipt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let key = &report.target;
    let (mailbox, uid, selected) = select_target(connection, key, Access::ReadWrite).await?;

    if !selected.permanent_flags_allow_new {
        return Err(ProviderError::invalid_state(format!(
            "{mailbox} does not permit new keywords (no \\* in PERMANENTFLAGS), so a report \
             keyword would be accepted and discarded"
        )));
    }

    let (set, clear) = keywords_for(report.verdict);
    let uid_set = uid.to_string();
    connection
        .uid_store(&uid_set, &format!("+FLAGS.SILENT ({set})"))
        .await?;
    connection
        .uid_store(&uid_set, &format!("-FLAGS.SILENT ({})", clear.join(" ")))
        .await?;

    let destination = report.destination.as_str();
    reject_control_chars(destination)?;
    // Only move when the message is not already where the verdict wants it; `UID MOVE`
    // into the mailbox that is currently selected is a copy-and-expunge onto itself on
    // some servers, and a no-op is cheaper than finding out which.
    if destination != mailbox {
        connection.uid_move(&uid_set, destination).await?;
    }

    Ok(ReportReceipt::new(ProviderKey::clone(key)))
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
