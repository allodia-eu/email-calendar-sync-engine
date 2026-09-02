//! Opening an authenticated IMAP session: TCP, TLS (implicit or `STARTTLS`),
//! authentication, and capability negotiation.
//!
//! Split from [`crate::provider`] (which is at the file-size limit) because it answers a
//! different question: that module is the [`Provider`](engine_provider::Provider)
//! surface a host drives, this one is how a session comes to exist at all. Two callers
//! share it, which is why it is not a method —
//! [`ImapProvider::connect`](crate::ImapProvider::connect) and
//! [`ImapWatcher::connect`](crate::ImapWatcher::connect), the latter needing its **own**
//! connection because a socket in `IDLE` cannot also `FETCH`.

use engine_provider::{ConnectObserver, ConnectStep, TlsVersion};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::{ImapConfig, ImapSecurity},
    credentials::Credentials,
    error::ImapError,
    tls_info,
    transport::Connection,
};

/// Opens a TCP + implicit-TLS connection, authenticates, and negotiates capabilities
/// (ENABLE QRESYNC + record IDLE) — the shared dial both
/// [`ImapProvider::connect`](crate::ImapProvider::connect) and
/// [`ImapWatcher::connect`](crate::watch::ImapWatcher::connect) build their session on.
/// Factored out so a watcher opens its **own** dedicated connection (push needs a
/// standing IDLE socket separate from the sync socket) without duplicating the
/// connect/authenticate/negotiate sequence or exposing the config's private fields.
///
/// Returns the session together with the TLS version its handshake agreed — the one
/// point where the concrete stream type is still visible, before it is erased behind
/// the generic [`Connection<S>`] (`tls_info`).
///
/// # Errors
///
/// [`ImapError`] on a TCP/TLS/authentication failure or a bad server name.
pub(crate) async fn connect_session(
    config: &ImapConfig,
    connector: &TlsConnector,
) -> Result<(Connection<TlsStream<TcpStream>>, Option<TlsVersion>), ImapError> {
    let tcp = TcpStream::connect(&config.addr).await?;
    let server_name = ServerName::try_from(config.server_name.clone())
        .map_err(|e| ImapError::bad(format!("invalid TLS server name: {e}")))?;
    // Implicit TLS wraps the socket now; STARTTLS runs the plaintext handshake command
    // first and upgrades the raw socket in place. Either way the result is one
    // `TlsStream`, so the rest of the dial — and every downstream type — is identical.
    let (tls, resumed) = match config.security {
        ImapSecurity::ImplicitTls => (connector.connect(server_name, tcp).await?, false),
        ImapSecurity::StartTls => {
            let mut plain = Connection::open(tcp).await?;
            plain.start_tls().await?;
            let upgraded = connector
                .connect(server_name, plain.into_inner_stream()?)
                .await?;
            (upgraded, true)
        }
    };
    let tls_version = tls_info::tls_version(&tls);
    // STARTTLS already consumed the one greeting on the plaintext connection, so it
    // resumes without reading another; implicit TLS reads its greeting here.
    let connection = if resumed {
        finish_session(Connection::resume(tls), tls_version, config).await?
    } else {
        open_session(tls, tls_version, config).await?
    };
    Ok((connection, tls_version))
}

/// Greets over an already-established `stream`, then authenticates and negotiates
/// capabilities via [`finish_session`]. The implicit-TLS / mock path (the STARTTLS
/// path uses [`finish_session`] directly, its greeting already read).
///
/// Generic over the stream, so the offline suite asserts the exact step sequence over
/// a `MockStream` — the handshake has already happened by the time this is called, so
/// passing `tls_version` in keeps the emitted order identical to the live dial's.
///
/// # Errors
///
/// [`ImapError`] on a greeting, authentication, or capability-negotiation failure.
pub(crate) async fn open_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    stream: S,
    tls_version: Option<TlsVersion>,
    config: &ImapConfig,
) -> Result<Connection<S>, ImapError> {
    finish_session(Connection::open(stream).await?, tls_version, config).await
}

/// Authenticates and negotiates the dialect over an already-greeted `connection`,
/// reporting [`ConnectStep::TlsEstablished`] (when the handshake agreed a version), then
/// [`ConnectStep::Authenticated`], then [`ConnectStep::Negotiated`] to the config's
/// observer. Shared by the implicit-TLS path ([`open_session`]) and the STARTTLS resume,
/// so both emit the same step order.
///
/// # Errors
///
/// [`ImapError`] on an authentication or capability-negotiation failure.
async fn finish_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut connection: Connection<S>,
    tls_version: Option<TlsVersion>,
    config: &ImapConfig,
) -> Result<Connection<S>, ImapError> {
    let observer: &dyn ConnectObserver = config.connect_observer();
    if let Some(version) = tls_version {
        observer.step(&ConnectStep::TlsEstablished(version));
    }
    // A password logs in; an access token goes over SASL, with the mechanism chosen
    // from what this server advertises (`crate::sasl`). Either way the next line is the
    // same: the observer is told the session is authenticated, not *how*.
    match &config.credentials {
        Credentials::Password { username, password } => {
            connection.login(username, password).await?;
        }
        Credentials::OAuth2 {
            username,
            access_token,
        } => {
            // `server_name` (not the dial `addr`) is the host an `OAUTHBEARER` response
            // names: it is the server's own name, which is what a loopback-mapped
            // fixture and a real deployment agree on.
            connection
                .authenticate_oauth2(username, access_token, &config.server_name, config.port())
                .await?;
        }
    }
    observer.step(&ConnectStep::Authenticated);
    // Settle the dialect and the extension set: `CAPABILITY`, then one `ENABLE` for
    // IMAP4rev2 where offered and for anything else that needs announcing. A server that
    // offers or confirms neither stays on the rev1 baseline.
    connection.negotiate().await?;
    // …and report what it agreed to. Two accounts on one build behave differently
    // because their servers agreed to different things, and this is the only line in the
    // trace that says which — the first thing to read on a support report.
    let (dialect, extensions) = connection.negotiated_summary();
    observer.step(&ConnectStep::negotiated(dialect, &extensions));
    Ok(connection)
}

#[cfg(test)]
#[path = "dial_tests.rs"]
mod tests;
