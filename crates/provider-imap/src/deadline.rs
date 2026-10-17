//! How long IMAP and SMTP wait on a server that has stopped answering.
//!
//! The bounds are the shared [`Deadlines`]; this module applies them to a socket:
//!
//! | Wait | Bound |
//! |---|---|
//! | TCP connect, TLS handshake | [`dial`](Deadlines::dial), each |
//! | an IMAP or SMTP greeting | [`greeting`](Deadlines::greeting), per line |
//! | every SMTP reply after the greeting and before the message | [`setup`](Deadlines::setup), per line |
//! | every line of an IMAP command's response | [`reply`](Deadlines::reply), per line |
//! | each read of a body (a `FETCH` literal) and each piece of a write (a command, an `APPEND` literal, an SMTP message) | [`stall`](Deadlines::stall), per piece |
//! | SMTP's reply to the final `.` | [`submission`](Deadlines::submission) |
//!
//! The one wait left open is a connection in `IDLE`, which is silent on purpose and is bounded
//! by the watch's own keep-alive (`crate::watch`).
//!
//! A bound that fires is an [`ImapError::Io`](crate::ImapError::Io) of kind
//! [`io::ErrorKind::TimedOut`], the same error a connection lost at that point gives, so it is
//! classified wherever that one is: a retryable failure everywhere but after an SMTP message
//! has been handed over, where the send is ambiguous (`smtp::converse`).

use std::{io, time::Duration};

use engine_provider::Deadlines;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::error::ImapResult;

/// The bounds IMAP and SMTP wait by: the engine's one set.
pub(crate) const BOUNDS: Deadlines = Deadlines::STANDARD;

/// How large a piece of a write is given its own bound.
///
/// Small enough that even a slow uplink moves one in seconds, so the bound is on a server that
/// has stopped reading, never on the size of what is being written.
const PIECE: usize = 8 * 1024;

/// Awaits `io`, failing with [`io::ErrorKind::TimedOut`] once `limit` passes. `waiting` names
/// what did not happen, for the error's text ("the server sent nothing").
pub(crate) async fn within<T>(
    limit: Duration,
    waiting: &str,
    io: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(limit, io).await.unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{waiting} for {}s", limit.as_secs()),
        ))
    })
}

/// Writes `bytes` and flushes, giving the server [`stall`](Deadlines::stall) to take each
/// piece, so a server that has stopped reading cannot hold a write once the socket buffers
/// are full.
pub(crate) async fn write(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    let waiting = "the server took nothing";
    for piece in bytes.chunks(PIECE) {
        within(BOUNDS.stall(), waiting, stream.write_all(piece)).await?;
    }
    within(BOUNDS.stall(), waiting, stream.flush()).await
}

/// Opens a TCP connection to `addr`, bounded by [`dial`](Deadlines::dial).
///
/// # Errors
///
/// [`ImapError::Io`](crate::ImapError::Io) when the connect fails or does not complete in time.
pub(crate) async fn connect(addr: &str) -> ImapResult<TcpStream> {
    let waiting = "the server did not accept the connection";
    Ok(within(BOUNDS.dial(), waiting, TcpStream::connect(addr)).await?)
}

/// Runs the TLS handshake over `tcp`, presenting `server_name`, bounded by
/// [`dial`](Deadlines::dial).
///
/// # Errors
///
/// [`ImapError::Io`](crate::ImapError::Io) when the handshake fails or does not complete in time.
pub(crate) async fn handshake(
    connector: &TlsConnector,
    server_name: ServerName<'static>,
    tcp: TcpStream,
) -> ImapResult<TlsStream<TcpStream>> {
    let waiting = "the server did not complete the TLS handshake";
    Ok(within(BOUNDS.dial(), waiting, connector.connect(server_name, tcp)).await?)
}

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "deadline_submit_tests.rs"]
mod submit_tests;

#[cfg(test)]
#[path = "deadline_imap_tests.rs"]
mod imap_tests;
