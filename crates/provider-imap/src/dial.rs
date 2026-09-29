//! Opening an authenticated IMAP session: TCP, TLS (implicit or `STARTTLS`),
//! authentication, and capability negotiation.
//!
//! Split from [`crate::provider`] (which is at the file-size limit) because it answers a
//! different question: that module is the [`Provider`](engine_provider::Provider)
//! surface a host drives, this one is how a session comes to exist at all. It is not a
//! method because it runs before there is anything to call one on: the first connection
//! [`ImapAccount::connect`](crate::ImapAccount::connect) opens, and every one the account's
//! pool opens after it, watches included.

use std::future::Future;

use engine_provider::{ConnectObserver, ConnectStep, TlsVersion};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::{ImapConfig, ImapSecurity},
    credentials::{Credentials, with_renewal},
    error::{ImapError, ImapResult},
    tls_info,
    transport::Connection,
};

/// Opens a TCP + TLS connection (implicit or `STARTTLS`), authenticates with the config's
/// [`Credentials`], and negotiates capabilities (ENABLE QRESYNC + record IDLE): the one
/// dial behind [`ImapAccount::connect`](crate::ImapAccount::connect) and every connection
/// the account's pool opens after it, so the first connection and the hundredth are made
/// the same way and present the same credential.
///
/// Returns the session together with the TLS version its handshake agreed — the one
/// point where the concrete stream type is still visible, before it is erased behind
/// the generic [`Connection<S>`] (`tls_info`).
///
/// # Errors
///
/// [`ImapError`] on a TCP/TLS/authentication failure or a bad server name, or
/// [`ImapError::Credential`] when the config's source has no credential to give.
pub(crate) async fn connect_session(
    config: &ImapConfig,
    connector: &TlsConnector,
) -> Result<(Connection<TlsStream<TcpStream>>, Option<TlsVersion>), ImapError> {
    dial_with(config, || {
        open_secured(
            &config.addr,
            &config.server_name,
            config.security,
            connector,
        )
    })
    .await
}

/// The authenticated half of a dial over whatever `open` connects: asks the config's
/// [`CredentialSource`](crate::CredentialSource) for a credential, and when the server
/// refuses it and the source renews it, opens a **fresh** connection and presents the
/// renewal. One re-dial at most, and none for a password, whose source never renews.
///
/// A new connection rather than a second `AUTHENTICATE` on the refused one, because a
/// server may close a connection after a failed sign-in, and one that does not has still
/// counted the attempt.
///
/// Generic over the stream so the offline suite drives it over scripted connections.
///
/// # Errors
///
/// As [`connect_session`].
pub(crate) async fn dial_with<S, F, Fut>(
    config: &ImapConfig,
    mut open: F,
) -> ImapResult<(Connection<S>, Option<TlsVersion>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    F: FnMut() -> Fut,
    Fut: Future<Output = ImapResult<(Connection<S>, Option<TlsVersion>)>>,
{
    with_renewal(config.credentials.as_ref(), |credentials| {
        let opening = open();
        async move {
            let (connection, tls_version) = opening.await?;
            let connection = finish_session(connection, tls_version, config, &credentials).await?;
            Ok((connection, tls_version))
        }
    })
    .await
}

/// Connects to `addr`, secures the socket, and reads the greeting: the half of a dial
/// that happens **before anyone says who they are**.
///
/// Split from [`connect_session`] because two callers need exactly this much and no
/// more. That one goes on to authenticate; [`crate::probe`] deliberately does not,
/// because "what would this server accept?" is asked at account setup, when there is no
/// credential to present yet.
///
/// # Errors
///
/// [`ImapError`] on a TCP/TLS/greeting failure or a bad server name.
pub(crate) async fn open_secured(
    addr: &str,
    server_name: &str,
    security: ImapSecurity,
    connector: &TlsConnector,
) -> Result<(Connection<TlsStream<TcpStream>>, Option<TlsVersion>), ImapError> {
    let tcp = TcpStream::connect(addr).await?;
    let server_name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| ImapError::bad(format!("invalid TLS server name: {e}")))?;
    // Implicit TLS wraps the socket now; STARTTLS runs the plaintext handshake command
    // first and upgrades the raw socket in place. Either way the result is one
    // `TlsStream`, so the rest of the dial — and every downstream type — is identical.
    let (tls, resumed) = match security {
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
        Connection::resume(tls)
    } else {
        Connection::open(tls).await?
    };
    Ok((connection, tls_version))
}

/// Authenticates with `credentials` and negotiates the dialect over an already-greeted
/// `connection`, reporting [`ConnectStep::TlsEstablished`] (when the handshake agreed a version),
/// then [`ConnectStep::Authenticated`], then [`ConnectStep::Negotiated`] to the config's
/// observer.
///
/// Generic over the stream, which is what lets the offline suite assert the exact step
/// sequence over a `MockStream`: the handshake has already happened by the time this is
/// called, so passing `tls_version` in keeps the emitted order identical to a live
/// dial's.
///
/// # Errors
///
/// [`ImapError`] on an authentication or capability-negotiation failure.
pub(crate) async fn finish_session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut connection: Connection<S>,
    tls_version: Option<TlsVersion>,
    config: &ImapConfig,
    credentials: &Credentials,
) -> Result<Connection<S>, ImapError> {
    let observer: &dyn ConnectObserver = config.connect_observer();
    if let Some(version) = tls_version {
        observer.step(&ConnectStep::TlsEstablished(version));
    }
    // A password logs in; an access token goes over SASL, with the mechanism chosen
    // from what this server advertises (`crate::sasl`). Either way the next line is the
    // same: the observer is told the session is authenticated, not *how*.
    match credentials {
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

#[cfg(test)]
#[path = "dial_renewal_tests.rs"]
mod renewal_tests;
