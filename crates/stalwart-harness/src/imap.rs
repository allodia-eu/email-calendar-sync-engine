//! Minimal IMAP probe: greeting, LOGIN, SELECT, SEARCH.
//!
//! The probe runs over any `Read + Write` stream so the transport (TLS in the
//! smoke suite, since Stalwart's IMAP listener is implicit-TLS on 993) stays out
//! of this crate's portable surface — the TLS wrapping is a test-only concern.
//! It asserts on content the harness controls (mailbox `EXISTS` counts and a
//! `SUBJECT` search), never on server-assigned UIDs.

use std::io::{Read, Write};

use crate::HarnessError;

/// Outcome of the IMAP probe against the seeded account.
#[derive(Debug, Clone)]
pub struct ImapProbe {
    /// The server greeting line (expected to start with `* OK`).
    pub greeting: String,
    /// Messages in INBOX (`SELECT INBOX` `EXISTS`) — the 8 seeded fixtures.
    pub inbox_exists: u32,
    /// Messages in Archive — the baseline message COPYed in (two memberships).
    pub archive_exists: u32,
    /// Messages in Projects — the message MOVEd in (single membership).
    pub projects_exists: u32,
    /// INBOX messages whose `SUBJECT` matches the duplicate-`Message-ID` pair —
    /// two are seeded, proving both are stored as distinct objects.
    pub dup_subject_hits: usize,
}

/// Drive the IMAP probe over an established (already TLS-wrapped) stream.
///
/// # Errors
/// Returns [`HarnessError::Protocol`] on an unexpected greeting or a command
/// that is not answered `OK`, and [`HarnessError::Io`] on a transport failure.
pub fn run_probe<S: Read + Write>(
    stream: S,
    account: &str,
    password: &str,
) -> Result<ImapProbe, HarnessError> {
    let mut conn = ImapStream::new(stream);

    let greeting = conn.read_line()?;
    if !greeting.starts_with("* OK") {
        return Err(protocol(format!("unexpected greeting: {greeting:?}")));
    }

    conn.command("a1", &format!("LOGIN \"{account}\" \"{password}\""))?;

    let inbox_exists = conn.select_exists("a2", "INBOX")?;
    let archive_exists = conn.select_exists("a3", "Archive")?;
    let projects_exists = conn.select_exists("a4", "Projects")?;

    // SEARCH runs against the selected mailbox, so reselect INBOX first.
    conn.select_exists("a5", "INBOX")?;
    let search = conn.command("a6", "SEARCH SUBJECT Duplicate")?;
    let dup_subject_hits = search
        .iter()
        .find_map(|line| line.strip_prefix("* SEARCH"))
        .map_or(0, |rest| rest.split_whitespace().count());

    let _ = conn.write_line("a7 LOGOUT");

    Ok(ImapProbe {
        greeting,
        inbox_exists,
        archive_exists,
        projects_exists,
        dup_subject_hits,
    })
}

fn protocol(detail: String) -> HarnessError {
    HarnessError::protocol("imap", detail)
}

fn io_err(source: std::io::Error) -> HarnessError {
    HarnessError::io("imap", source)
}

/// A line-buffered IMAP conversation over a single read+write stream.
struct ImapStream<S> {
    inner: S,
    pending: Vec<u8>,
}

impl<S: Read + Write> ImapStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    fn write_line(&mut self, line: &str) -> Result<(), HarnessError> {
        self.inner
            .write_all(line.as_bytes())
            .and_then(|()| self.inner.write_all(b"\r\n"))
            .map_err(io_err)
    }

    fn read_line(&mut self) -> Result<String, HarnessError> {
        loop {
            if let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=nl).collect();
                return Ok(String::from_utf8_lossy(&line).trim_end().to_owned());
            }
            let mut chunk = [0u8; 1024];
            let n = self.inner.read(&mut chunk).map_err(io_err)?;
            if n == 0 {
                return Err(protocol("connection closed mid-response".to_owned()));
            }
            self.pending.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send a tagged command and read response lines through its completion.
    fn command(&mut self, tag: &str, cmd: &str) -> Result<Vec<String>, HarnessError> {
        self.write_line(&format!("{tag} {cmd}"))?;
        let mark = format!("{tag} "); // the tagged-completion prefix, built once
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            let done = line.starts_with(&mark);
            lines.push(line);
            if done {
                break;
            }
        }
        match lines.last() {
            Some(last) if last.starts_with(&format!("{tag} OK")) => Ok(lines),
            other => Err(protocol(format!("{tag} {cmd} not OK: {other:?}"))),
        }
    }

    /// `SELECT` a mailbox and return its `EXISTS` count.
    fn select_exists(&mut self, tag: &str, mailbox: &str) -> Result<u32, HarnessError> {
        let lines = self.command(tag, &format!("SELECT {mailbox}"))?;
        lines
            .iter()
            .find_map(|line| line.strip_prefix("* ").and_then(parse_exists))
            .ok_or_else(|| protocol(format!("SELECT {mailbox} returned no EXISTS")))
    }
}

/// Parse the count out of an `n EXISTS` untagged line body (after `* `).
fn parse_exists(body: &str) -> Option<u32> {
    let (count, kind) = body.split_once(' ')?;
    (kind == "EXISTS").then(|| count.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::{ImapStream, parse_exists, run_probe};
    use std::io::{Cursor, Read, Write};

    #[test]
    fn parses_exists_count() {
        assert_eq!(parse_exists("8 EXISTS"), Some(8));
        assert_eq!(parse_exists("3 RECENT"), None);
        assert_eq!(parse_exists("OK [UIDVALIDITY 1]"), None);
    }

    // A canned stream: reads come from `script`, writes are discarded. Lets the
    // probe logic be exercised offline without a server.
    struct Canned {
        script: Cursor<Vec<u8>>,
    }
    impl Read for Canned {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.script.read(buf)
        }
    }
    impl Write for Canned {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn canned(s: &str) -> Canned {
        Canned {
            script: Cursor::new(s.replace('\n', "\r\n").into_bytes()),
        }
    }

    #[test]
    fn read_line_splits_on_crlf() {
        let mut s = ImapStream::new(canned("* OK hi\na1 OK done\n"));
        assert_eq!(s.read_line().unwrap(), "* OK hi");
        assert_eq!(s.read_line().unwrap(), "a1 OK done");
    }

    #[test]
    fn command_rejects_non_ok() {
        let mut s = ImapStream::new(canned("a1 NO nope\n"));
        assert!(s.command("a1", "LOGIN x y").is_err());
    }

    #[test]
    fn run_probe_parses_a_full_session() {
        let script = "\
* OK [CAPABILITY IMAP4rev2] Stalwart\n\
a1 OK login ok\n\
* 8 EXISTS\na2 OK selected\n\
* 1 EXISTS\na3 OK selected\n\
* 1 EXISTS\na4 OK selected\n\
* 8 EXISTS\na5 OK selected\n\
* SEARCH 2 3\na6 OK search done\n\
a7 OK bye\n";
        let probe = run_probe(canned(script), "alice@test.local", "pw").unwrap();
        assert_eq!(probe.inbox_exists, 8);
        assert_eq!(probe.archive_exists, 1);
        assert_eq!(probe.projects_exists, 1);
        assert_eq!(probe.dup_subject_hits, 2);
    }
}
