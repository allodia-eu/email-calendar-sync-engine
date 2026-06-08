//! Minimal SMTP probe over raw TCP: read the banner, `EHLO`, then `QUIT`.
//!
//! Uses a single stream (no `try_clone`) and a byte-wise line reader, matching
//! the IMAP/HTTP probes. SMTP is server-speaks-first, so it reads the `220`
//! banner before sending anything.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::HarnessError;

/// Connect, read the `220` banner, confirm `EHLO` is answered `250`, and quit.
///
/// Returns the banner line on success.
pub(crate) fn banner(addr: &str) -> Result<String, HarnessError> {
    let protocol = |detail| HarnessError::protocol("smtp", detail);

    // Plain blocking connect + blocking reads, matching the IMAP probe. A
    // socket read timeout (SO_RCVTIMEO) was observed not to wake on arriving
    // data through Docker Desktop's macOS port proxy (Python's select-based
    // timeout and a blocking read both work), so we don't set one; the server
    // is health-gated before the probe runs.
    let mut stream = TcpStream::connect(addr).map_err(|s| io(addr, s))?;

    // Send EHLO before reading. SMTP is server-speaks-first, but a userspace TCP
    // proxy (Docker Desktop on macOS) may not dial the backend — and so never
    // surface the `220` banner — until the client writes. Writing first primes
    // it; on a direct connection (Linux/CI) the banner is simply the first
    // response line, so the read order is unchanged. Stalwart tolerates the
    // early EHLO.
    stream
        .write_all(b"EHLO harness.test.local\r\n")
        .map_err(|s| io(addr, s))?;

    let banner = read_line(&mut stream, addr)?;
    if !banner.starts_with("220") {
        return Err(protocol(format!("unexpected banner: {banner:?}")));
    }

    loop {
        let line = read_line(&mut stream, addr)?;
        if !line.starts_with("250") {
            return Err(protocol(format!("EHLO not accepted: {line:?}")));
        }
        // A space at index 3 (`250 `) marks the final line; `250-` continues.
        if line.as_bytes().get(3) == Some(&b' ') {
            break;
        }
    }

    let _ = stream.write_all(b"QUIT\r\n");
    Ok(banner)
}

fn io(addr: &str, source: std::io::Error) -> HarnessError {
    HarnessError::io(addr, source)
}

/// Read one CRLF-terminated line from the stream, one byte at a time so no bytes
/// past the line are consumed.
fn read_line(stream: &mut TcpStream, addr: &str) -> Result<String, HarnessError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).map_err(|s| io(addr, s))?;
        if n == 0 {
            return Err(HarnessError::protocol("smtp", "connection closed mid-line"));
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_owned())
}
