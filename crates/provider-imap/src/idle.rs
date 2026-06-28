//! IDLE (RFC 2177) transport primitives — the standing-connection push mechanism
//! [`crate::watch::ImapWatcher`] drives.
//!
//! IDLE falls outside [`Connection::command`](crate::transport)'s tagged
//! request/response shape: the client sends `<tag> IDLE`, the server answers with a
//! `+ ` **continuation** (not a tagged completion) and then streams *unsolicited*
//! untagged responses for as long as the client idles, until the client sends a bare
//! `DONE` to end it. So these three primitives manage that lifecycle directly over the
//! connection's low-level read/write seam:
//!
//! - [`idle_start`] — send `IDLE`, consume the continuation, return the command tag.
//! - [`idle_wait_change`] — read untagged responses until one signals a change.
//! - [`idle_done`] — send `DONE`, drain to the tagged completion, report a boundary
//!   change.
//!
//! A notification carries **no data** — it only classifies *that* the mailbox changed
//! (see [`classify`]); the watcher turns that into a [`WatchEvent`](engine_provider::WatchEvent)
//! and the host responds by running the scope's normal sync (`crate::watch` docs).

use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{ImapError, ImapResult};
use crate::transport::{Connection, strip_ascii_prefix};

/// What an untagged response received while idling means to the watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleLine {
    /// A change notification — `* n EXISTS` (new mail), `* n EXPUNGE` (a delete),
    /// `* n FETCH (…)` (a flag change), or `* VANISHED …` (QRESYNC expunges). The
    /// mailbox changed; the watcher reports [`WatchEvent::Changed`](engine_provider::WatchEvent::Changed).
    Changed,
    /// Informational — `* n RECENT`, `* OK …` (a server "still here" poke), or any
    /// other untagged status. No action.
    Informational,
    /// `* BYE` — the server is closing the connection; the watch must reconnect.
    Bye,
    /// Not an untagged response at all (a tagged line). Unexpected mid-IDLE — the
    /// server completed or refused the command out of band; surfaced as an error.
    Unexpected,
}

/// Classifies one server line (raw, with its `* ` prefix and trailing CRLF) seen
/// while idling. Pure and panic-resistant on hostile input, like the rest of the
/// parse layer: it only inspects the first one or two whitespace tokens.
fn classify(line: &[u8]) -> IdleLine {
    let Some(body) = strip_ascii_prefix(line, b"* ") else {
        return IdleLine::Unexpected;
    };
    let text = String::from_utf8_lossy(body);
    let mut tokens = text.split_whitespace();
    let Some(first) = tokens.next() else {
        return IdleLine::Informational; // a bare `* ` — nothing to act on
    };
    if first.eq_ignore_ascii_case("BYE") {
        return IdleLine::Bye;
    }
    if first.eq_ignore_ascii_case("VANISHED") {
        return IdleLine::Changed;
    }
    // The numeric forms: `* <n> EXISTS|EXPUNGE|FETCH` are changes; `* <n> RECENT`
    // (and anything else) is informational. A non-numeric, non-VANISHED/BYE head
    // (`OK`, `FLAGS`, `CAPABILITY`, …) is informational.
    if first.parse::<u64>().is_ok() {
        return match tokens.next() {
            Some(kind)
                if kind.eq_ignore_ascii_case("EXISTS")
                    || kind.eq_ignore_ascii_case("EXPUNGE")
                    || kind.eq_ignore_ascii_case("FETCH") =>
            {
                IdleLine::Changed
            }
            _ => IdleLine::Informational,
        };
    }
    IdleLine::Informational
}

/// Sends `<tag> IDLE` and consumes the server's `+ ` continuation, returning the tag
/// (so [`idle_done`] can match the command's eventual completion). Untagged responses
/// the server interleaves before the continuation are skipped — a change in that
/// sub-second window is reconciled by the host's sync-on-start and the keep-alive
/// backstop (`crate::watch`), never lost silently into the store.
///
/// # Errors
///
/// [`ImapError::Protocol`] if the server answers with a tagged line instead of a
/// continuation (it refused `IDLE`), or [`ImapError::Io`] on a transport failure.
pub(crate) async fn idle_start<S>(conn: &mut Connection<S>) -> ImapResult<String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let tag = conn.next_tag();
    conn.send_raw(format!("{tag} IDLE\r\n").as_bytes()).await?;
    loop {
        let line = conn.read_line().await?;
        if strip_ascii_prefix(&line, b"+ ").is_some() {
            return Ok(tag);
        }
        if strip_ascii_prefix(&line, b"* ").is_some() {
            continue; // an untagged status before the continuation; tolerated
        }
        return Err(ImapError::protocol(format!(
            "IDLE expected a continuation, got: {}",
            String::from_utf8_lossy(&line).trim()
        )));
    }
}

/// Reads untagged responses until one signals a change, returning then while the
/// connection **stays in IDLE** (informational lines are consumed and ignored). The
/// caller bounds the wait with a keep-alive timeout (`crate::watch`); because
/// [`Connection::read_line`](crate::transport) is cancel-safe, a timeout that drops
/// this future loses no buffered bytes.
///
/// # Errors
///
/// [`ImapError::Bye`] if the server sends `* BYE`, [`ImapError::Protocol`] on an
/// unexpected tagged line mid-IDLE, or [`ImapError::Io`] if the connection drops.
pub(crate) async fn idle_wait_change<S>(conn: &mut Connection<S>) -> ImapResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    loop {
        let line = conn.read_line().await?;
        match classify(&line) {
            IdleLine::Changed => return Ok(()),
            IdleLine::Informational => {} // consume and keep reading the IDLE stream
            IdleLine::Bye => {
                return Err(ImapError::bye(
                    String::from_utf8_lossy(&line).trim().to_owned(),
                ));
            }
            IdleLine::Unexpected => {
                return Err(ImapError::protocol(format!(
                    "unexpected line while idling: {}",
                    String::from_utf8_lossy(&line).trim()
                )));
            }
        }
    }
}

/// Sends a bare `DONE` to end IDLE and drains to the command's tagged completion,
/// returning whether any **change** notification arrived in the drain — so the watcher
/// can convert a change that landed right at the keep-alive boundary into a
/// [`WatchEvent::Changed`](engine_provider::WatchEvent::Changed) rather than swallow it
/// as a plain keep-alive. `tag` is the value [`idle_start`] returned.
///
/// # Errors
///
/// [`ImapError::Bye`] if the server closes with `* BYE` mid-drain, [`ImapError::No`]/
/// [`ImapError::Bad`] on a non-`OK` IDLE completion, [`ImapError::Protocol`] on an
/// unknown completion status, or [`ImapError::Io`] on a transport failure.
pub(crate) async fn idle_done<S>(conn: &mut Connection<S>, tag: &str) -> ImapResult<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    conn.send_raw(b"DONE\r\n").await?;
    let prefix = format!("{tag} ");
    let mut saw_change = false;
    loop {
        let line = conn.read_line().await?;
        let text = String::from_utf8_lossy(&line);
        if let Some(rest) = text.strip_prefix(&prefix) {
            let mut parts = rest.trim_end().splitn(2, ' ');
            let status = parts.next().unwrap_or_default();
            let detail = parts.next().unwrap_or_default().to_owned();
            return match status.to_ascii_uppercase().as_str() {
                "OK" => Ok(saw_change),
                "NO" => Err(ImapError::no(detail)),
                "BAD" => Err(ImapError::bad(detail)),
                other => Err(ImapError::protocol(format!(
                    "unknown IDLE completion {other}"
                ))),
            };
        }
        match classify(&line) {
            IdleLine::Changed => saw_change = true,
            IdleLine::Bye => return Err(ImapError::bye(text.trim().to_owned())),
            IdleLine::Informational | IdleLine::Unexpected => {}
        }
    }
}

#[cfg(test)]
#[path = "idle_tests.rs"]
mod tests;
