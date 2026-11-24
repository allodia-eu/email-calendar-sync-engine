//! A ManageSieve session (RFC 5804) over any async stream: the greeting, `STARTTLS`, and the
//! commands a rule editor needs.
//!
//! Generic over the stream, as the IMAP [`Connection`](crate::transport::Connection) is, so the
//! offline tests drive it over a scripted [`MockStream`](crate::mock::MockStream) with the bytes
//! the harness servers sent, and the live client over `tokio-rustls`. Every read and write is
//! bounded by the engine's deadlines (`crate::deadline`). ManageSieve has no command tags: one
//! command is in flight at a time, and its response ends at the first `OK`, `NO` or `BYE`.
//!
//! Signing in is [`crate::managesieve::auth`]; dialling, with TLS and the credential source, is
//! [`crate::managesieve::dial`].

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::io::{AsyncRead, AsyncWrite, BufReader};

use super::{
    error::{SieveError, SieveResult},
    name::ScriptName,
    response::{Capabilities, Completion, Line, Status, Token, parse_line},
};
use crate::{
    deadline::{self, BOUNDS},
    error::ImapError,
    transport_read::{LineLimits, read_framed_line},
};

/// How much one logical line may hold: a literal of at most 4 MiB (Sieve scripts are far smaller:
/// Stalwart refuses one over 100 KiB, Dovecot over 1 MiB by default), and 64 KiB besides. A larger
/// literal is refused before anything is allocated for it, a longer line before its bytes are kept.
const LIMITS: LineLimits = LineLimits {
    max_literal: 4 * 1024 * 1024,
    max_line: Some(4 * 1024 * 1024 + 64 * 1024),
    literal_needs_crlf: true,
};

/// The most bytes one response may hold, all its lines together: a response is read whole before
/// it is answered, so a server that never completes one is cut off here.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// One script as `LISTSCRIPTS` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedScript {
    pub(crate) name: ScriptName,
    pub(crate) active: bool,
    /// Whether its name is one RFC 5804 §1.6 allows: a script that is not is counted, never
    /// addressed.
    pub(crate) usable: bool,
}

/// A command's data lines, read up to an `OK`.
type DataLines = Vec<Vec<Token>>;

/// A ManageSieve session.
pub(crate) struct Session<S> {
    pub(super) inner: BufReader<S>,
    capabilities: Capabilities,
    /// The bytes the response being read has held so far ([`MAX_RESPONSE`]).
    received: usize,
    /// Whether the stream failed under the session ([`broken`](Self::broken)).
    broken: bool,
    /// Set once a write (`PUTSCRIPT`, `SETACTIVE`) goes out, so a caller that gives up on the
    /// session afterwards knows the write may have landed ([`watch_writes`](Self::watch_writes)).
    wrote: Arc<AtomicBool>,
}

impl<S> core::fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Session<S> {
    /// Wraps a stream and reads the capabilities the server sends unasked, then `OK`: on
    /// connect (its greeting), and again once `STARTTLS` has upgraded the stream (RFC 5804 §2.2),
    /// when it offers what it keeps for TLS, the SASL mechanisms among them.
    ///
    /// # Errors
    ///
    /// [`ImapError::Bye`] when the server greets with `BYE`, [`ImapError::Protocol`] for a
    /// greeting that is not ManageSieve, and [`ImapError::Io`] on the transport.
    pub(crate) async fn open(stream: S) -> SieveResult<Self> {
        let mut session = Self {
            inner: BufReader::new(stream),
            capabilities: Capabilities::default(),
            received: 0,
            broken: false,
            wrote: Arc::default(),
        };
        let lines = session.read_ok("the greeting").await?;
        session.capabilities = Capabilities::from_lines(&lines)?;
        Ok(session)
    }

    /// What the server said it offers, last time it said.
    pub(crate) fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Asks the server what it offers now, and keeps the answer: once signed in a server may
    /// say more (Dovecot adds `MAXREDIRECTS`).
    ///
    /// # Errors
    ///
    /// The command's failure, classified.
    pub(crate) async fn refresh_capabilities(&mut self) -> SieveResult<&Capabilities> {
        let lines = self.command(b"CAPABILITY", "CAPABILITY").await?;
        self.capabilities = Capabilities::from_lines(&lines)?;
        Ok(&self.capabilities)
    }

    /// Runs the plaintext half of `STARTTLS`: refuses unless the server offers it, then asks
    /// for it. The caller upgrades the socket next
    /// ([`into_inner_stream`](Self::into_inner_stream)).
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] when `STARTTLS` is not offered: a credential never crosses a link
    /// that cannot be upgraded. Otherwise the command's failure.
    pub(crate) async fn start_tls(&mut self) -> SieveResult<()> {
        if !self.capabilities.starttls {
            return Err(ImapError::protocol(
                "the ManageSieve server does not offer STARTTLS; refusing to sign in in the clear",
            )
            .into());
        }
        self.command(b"STARTTLS", "STARTTLS").await?;
        Ok(())
    }

    /// The raw stream after `STARTTLS`, for the TLS handshake.
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] if the server sent anything past its answer to `STARTTLS`:
    /// plaintext it injected before the handshake must not be read as if it came over TLS
    /// (CVE-2011-0411's class), so it is dropped with the connection.
    pub(crate) fn into_inner_stream(self) -> SieveResult<S> {
        if !self.inner.buffer().is_empty() {
            return Err(ImapError::protocol(
                "unexpected buffered data after STARTTLS (possible command injection)",
            )
            .into());
        }
        Ok(self.inner.into_inner())
    }

    /// Every script of the account, and which is active.
    ///
    /// A script whose name RFC 5804 §1.6 does not allow (a control character, U+2028, U+2029) is
    /// listed as not usable, so one such name does not hide the rest and still counts: two scripts
    /// never read as one. Where it would be read or written, the caller refuses.
    ///
    /// # Errors
    ///
    /// The command's failure; [`ImapError::Protocol`] for a line that is not a script name.
    pub(crate) async fn list_scripts(&mut self) -> SieveResult<Vec<ListedScript>> {
        let lines = self.command(b"LISTSCRIPTS", "LISTSCRIPTS").await?;
        let listed = lines
            .into_iter()
            .map(|line| {
                let (name, active) = match line.as_slice() {
                    [Token::Str(name)] => (name, false),
                    [Token::Str(name), Token::Atom(flag)]
                        if flag.eq_ignore_ascii_case("ACTIVE") =>
                    {
                        (name, true)
                    }
                    _ => {
                        return Err(ImapError::protocol(
                            "a LISTSCRIPTS line that is not a script name",
                        )
                        .into());
                    }
                };
                Ok(((ScriptName::new(name), name.clone()), active))
            })
            .collect::<SieveResult<Vec<_>>>()?;
        Ok(listed
            .into_iter()
            .map(|((name, raw), active)| match name {
                Ok(name) => ListedScript {
                    name,
                    active,
                    usable: true,
                },
                Err(_) => ListedScript {
                    name: ScriptName::forbidden(&raw),
                    active,
                    usable: false,
                },
            })
            .collect())
    }

    /// The text of the script `name`.
    ///
    /// # Errors
    ///
    /// [`SieveError::Refused`] with `NONEXISTENT` when there is no such script; otherwise the
    /// command's failure.
    pub(crate) async fn get_script(&mut self, name: &ScriptName) -> SieveResult<String> {
        let command = command_with(b"GETSCRIPT ", &[name.as_str().as_bytes()]);
        let lines = self.command(&command, "GETSCRIPT").await?;
        match lines.as_slice() {
            [line] => match line.as_slice() {
                [Token::Str(text)] => Ok(text.clone()),
                _ => Err(ImapError::protocol("a GETSCRIPT answer that is not one string").into()),
            },
            _ => Err(ImapError::protocol("a GETSCRIPT answer that is not one string").into()),
        }
    }

    /// Stores `text` as the script `name`, replacing one of that name. The server checks it
    /// first and stores nothing it refuses (RFC 5804 §2.6).
    ///
    /// # Errors
    ///
    /// [`SieveError::Refused`]: no code for a script the server finds invalid, `QUOTA/…` for one
    /// it has no room for. Otherwise the command's failure.
    pub(crate) async fn put_script(&mut self, name: &ScriptName, text: &str) -> SieveResult<()> {
        let command = command_with(b"PUTSCRIPT ", &[name.as_str().as_bytes(), text.as_bytes()]);
        self.wrote.store(true, Ordering::SeqCst);
        self.command(&command, "PUTSCRIPT").await?;
        Ok(())
    }

    /// Asks the server whether it would accept `text`, storing nothing (RFC 5804 §2.12). Only the
    /// live suite sends it, to prove a server refuses what its vocabulary leaves out: a write
    /// relies on `PUTSCRIPT`'s own check instead.
    #[cfg(test)]
    /// # Errors
    ///
    /// [`SieveError::Refused`] when it would not; otherwise the command's failure.
    pub(crate) async fn check_script(&mut self, text: &str) -> SieveResult<()> {
        let command = command_with(b"CHECKSCRIPT ", &[text.as_bytes()]);
        self.command(&command, "CHECKSCRIPT").await?;
        Ok(())
    }

    /// Asks whether a script of `size` bytes named `name` would fit (RFC 5804 §2.3). The engine
    /// does not ask before storing: Stalwart answers yes to any size and refuses the store
    /// (`docs/spec-deviations.md`, STALWART-NEW-1), so only the live suite sends it, to pin that.
    #[cfg(test)]
    pub(crate) async fn have_space(&mut self, name: &ScriptName, size: usize) -> SieveResult<()> {
        let mut command = command_with(b"HAVESPACE ", &[name.as_str().as_bytes()]);
        command.extend_from_slice(format!(" {size}").as_bytes());
        self.command(&command, "HAVESPACE").await?;
        Ok(())
    }

    /// Makes `name` the active script, or, with `None`, leaves none active.
    ///
    /// # Errors
    ///
    /// The command's failure.
    pub(crate) async fn set_active(&mut self, name: Option<&ScriptName>) -> SieveResult<()> {
        let name = name.map_or("", ScriptName::as_str);
        let command = command_with(b"SETACTIVE ", &[name.as_bytes()]);
        self.wrote.store(true, Ordering::SeqCst);
        self.command(&command, "SETACTIVE").await?;
        Ok(())
    }

    /// Deletes the script `name`. Only the tests delete a script: the engine never does.
    #[cfg(test)]
    /// # Errors
    ///
    /// [`SieveError::Refused`] with `ACTIVE` for the active script, `NONEXISTENT` for none;
    /// otherwise the command's failure.
    pub(crate) async fn delete_script(&mut self, name: &ScriptName) -> SieveResult<()> {
        let command = command_with(b"DELETESCRIPT ", &[name.as_str().as_bytes()]);
        self.command(&command, "DELETESCRIPT").await?;
        Ok(())
    }

    /// Sends `LOGOUT` and does not wait for its answer: for a session that read what it came for
    /// and owes the server nothing.
    pub(crate) async fn leave(mut self) {
        let _ = self.send(b"LOGOUT\r\n").await;
    }

    /// Ends the session politely. The work is done by then, so a failure here changes nothing.
    pub(crate) async fn logout(&mut self) {
        let _ = self.command(b"LOGOUT", "LOGOUT").await;
    }

    /// Sends one command line and reads its response to an `OK`.
    async fn command(&mut self, command: &[u8], name: &'static str) -> SieveResult<DataLines> {
        let mut line = Vec::with_capacity(command.len() + 2);
        line.extend_from_slice(command);
        line.extend_from_slice(b"\r\n");
        self.send(&line).await?;
        self.read_ok(name).await
    }

    /// Writes bytes, a piece at a time under the stall bound (`crate::deadline::write`).
    pub(super) async fn send(&mut self, bytes: &[u8]) -> SieveResult<()> {
        // Whatever is read next answers what is sent now: its count starts again.
        self.received = 0;
        let sent = deadline::write(&mut self.inner, bytes).await;
        self.broken |= sent.is_err();
        Ok(sent?)
    }

    /// Shares the flag a write sets as it goes out, so it can be read after the session is gone.
    pub(crate) fn watch_writes(&mut self, wrote: Arc<AtomicBool>) {
        self.wrote = wrote;
    }

    /// Whether the stream failed under the session (I/O, a timeout, an answer it could not
    /// frame or read): nothing more is sent on it, `LOGOUT` included.
    pub(crate) fn broken(&self) -> bool {
        self.broken
    }

    /// Reads one logical line, its literals inlined, under the reply bound.
    pub(super) async fn read_line(&mut self) -> SieveResult<Line> {
        let line = read_framed_line(&mut self.inner, Some(BOUNDS.reply()), LIMITS)
            .await
            .inspect_err(|_| self.broken = true)?;
        self.received = self.received.saturating_add(line.len());
        if self.received > MAX_RESPONSE {
            self.broken = true;
            return Err(ImapError::protocol(format!(
                "a ManageSieve response exceeding the {MAX_RESPONSE}-byte cap"
            ))
            .into());
        }
        Ok(parse_line(&line).inspect_err(|_| self.broken = true)?)
    }

    /// Reads a response's data lines up to its completion: `OK` answers them, `NO` is a refusal
    /// of `name`, `BYE` the server closing.
    async fn read_ok(&mut self, name: &'static str) -> SieveResult<DataLines> {
        let mut lines = Vec::new();
        loop {
            match self.read_line().await? {
                Line::Data(tokens) => lines.push(tokens),
                Line::Completion(completion) => {
                    completed(name, completion)?;
                    return Ok(lines);
                }
            }
        }
    }
}

/// A completion as the outcome of `command`.
pub(super) fn completed(command: &'static str, completion: Completion) -> SieveResult<()> {
    match completion.status {
        Status::Ok => Ok(()),
        Status::No => Err(SieveError::Refused {
            command,
            code: completion.code,
        }),
        Status::Bye => Err(ImapError::bye(format!(
            "the ManageSieve server closed the connection during {command}"
        ))
        .into()),
    }
}

/// `head` followed by each argument as a string: quoted where it can be, a non-synchronizing
/// literal (`{n+}`, which every server takes, RFC 5804 §4) where it holds a line end, a quote
/// or a backslash, or is long. A script always goes as a literal.
fn command_with(head: &[u8], arguments: &[&[u8]]) -> Vec<u8> {
    let mut command = head.to_vec();
    for (at, argument) in arguments.iter().enumerate() {
        if at > 0 {
            command.push(b' ');
        }
        let quotable = argument.len() <= 1024
            && !argument
                .iter()
                .any(|&byte| matches!(byte, b'\r' | b'\n' | b'\0' | b'"' | b'\\'));
        if quotable {
            command.push(b'"');
            command.extend_from_slice(argument);
            command.push(b'"');
        } else {
            command.extend_from_slice(format!("{{{}+}}\r\n", argument.len()).as_bytes());
            command.extend_from_slice(argument);
        }
    }
    command
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
