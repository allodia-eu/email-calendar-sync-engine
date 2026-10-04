//! How long a dial or a submission waits on a server that has stopped answering.
//!
//! A server that accepted the connection and then says nothing (a paused process behind a
//! port forwarder, a captive portal, a stalled middlebox, a NAT mapping that died) reports
//! nothing either, and a read waits on it forever. Bounded here: every TCP connect and TLS
//! handshake, the IMAP and SMTP greetings, and the whole SMTP conversation, probes included.
//! A body read has its own bound
//! ([`BODY_READ_STALL`](crate::transport_read::BODY_READ_STALL)).
//!
//! A bound that fires is an [`ImapError::Io`](crate::ImapError::Io) of kind
//! [`io::ErrorKind::TimedOut`], the same error a connection lost at that point gives, so it is
//! classified wherever that one is: a retryable failure before an SMTP message has been handed
//! over, and an ambiguous send once it has (`smtp::converse`).

use std::{io, time::Duration};

use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::error::ImapResult;

/// How long a TCP connect, and then the TLS handshake over it, may each take.
///
/// Both are a few round trips to a server that is up, so half a minute leaves room for a
/// congested mobile link and none for a server that is not answering. Unbounded, a connect
/// waits out the OS's own retries (over two minutes on Linux) and a handshake waits forever.
pub(crate) const DIAL_STALL: Duration = Duration::from_secs(30);

/// How long a server may stay silent when it owes the client an answer: an IMAP or SMTP
/// greeting, and every SMTP reply before the message has been handed over. It also bounds
/// each piece of an SMTP write, so a server that has stopped reading cannot hold one.
///
/// RFC 5321 §4.5.3.2 asks for five minutes per command, written for relays between busy
/// transfer agents. A submission server answers an interactive client in well under a
/// second, and a greeting delayed against spam is a matter of seconds, so a minute of
/// nothing is not a slow server. Until this fires the send is in nobody's queue: the host
/// cannot retry it and the user cannot edit it.
pub(crate) const REPLY_STALL: Duration = Duration::from_mins(1);

/// How long an SMTP server may take to acknowledge the end of a message (its reply to the
/// final `.`).
///
/// Ten minutes, as RFC 5321 §4.5.3.2.6 asks, because the server may still be filtering the
/// message and this is the one wait whose expiry cannot say what happened: the message may
/// have been accepted, so the send becomes a question for the user rather than a retry.
/// A shorter bound would ask that question of a server that was merely slow.
pub(crate) const DATA_ACK_STALL: Duration = Duration::from_mins(10);

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

/// Opens a TCP connection to `addr`, bounded by [`DIAL_STALL`].
///
/// # Errors
///
/// [`ImapError::Io`](crate::ImapError::Io) when the connect fails or does not complete in time.
pub(crate) async fn connect(addr: &str) -> ImapResult<TcpStream> {
    let waiting = "the server did not accept the connection";
    Ok(within(DIAL_STALL, waiting, TcpStream::connect(addr)).await?)
}

/// Runs the TLS handshake over `tcp`, presenting `server_name`, bounded by [`DIAL_STALL`].
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
    Ok(within(DIAL_STALL, waiting, connector.connect(server_name, tcp)).await?)
}

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "deadline_submit_tests.rs"]
mod submit_tests;
