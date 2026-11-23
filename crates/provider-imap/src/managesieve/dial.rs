//! Opening a ManageSieve session: TCP, the greeting, `STARTTLS`, TLS, and signing in with a
//! credential the account's source gives.
//!
//! RFC 5804 defines one transport, `STARTTLS` on a plain connection (port 4190), and no
//! implicit-TLS port, so there is no choice to configure. The endpoint is the host's
//! ([`ImapConfig::with_managesieve`](crate::ImapConfig::with_managesieve)): no SRV discovery
//! (RFC 5804 §1.8) for now. The TLS connector is the account's, so the host's trust policy
//! covers this socket as it covers IMAP's.

use std::future::Future;

use engine_provider::{CredentialProtocol, Origin};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use super::{
    error::{SieveError, SieveResult},
    session::Session,
};
use crate::{
    config::{ImapConfig, port_of},
    deadline::{self, BOUNDS},
    error::ImapError,
};

/// The port RFC 5804 §1.8 assigns, used when the configured address names none.
const DEFAULT_PORT: u16 = 4190;

/// Where an account's ManageSieve service is, as the host configured it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SieveEndpoint {
    /// `host:port` to dial.
    pub(crate) addr: String,
    /// The name the TLS handshake verifies, and the host an `OAUTHBEARER` response names.
    pub(crate) server_name: String,
}

impl SieveEndpoint {
    /// The port from the address, where it names one.
    fn port(&self) -> Option<u16> {
        port_of(&self.addr)
    }

    /// Where a Sieve sign-in goes, as the credential source is told: the `sieve` scheme (RFC 5804
    /// §3), the TLS server name, and the port.
    pub(crate) fn origin(&self) -> Origin {
        Origin::new(
            "sieve",
            &self.server_name,
            self.port().unwrap_or(DEFAULT_PORT),
        )
    }
}

/// A signed-in session at `endpoint`, with a credential from `config`'s source: asked for
/// [`CredentialProtocol::ManageSieve`] at the endpoint's [origin](SieveEndpoint::origin), and
/// replaced once when the server refuses it, on a fresh connection.
///
/// # Errors
///
/// As [`dial_with`].
pub(crate) async fn connect(
    config: &ImapConfig,
    endpoint: &SieveEndpoint,
    connector: &TlsConnector,
) -> SieveResult<Session<TlsStream<TcpStream>>> {
    dial_with(config, endpoint, || open_tls(endpoint, connector)).await
}

/// The signed-in half of a dial, over whatever `open` connects (a scripted stream offline).
///
/// # Errors
///
/// The credential source's failure, the transport's, or the sign-in's
/// ([`Session::authenticate`]).
pub(crate) async fn dial_with<S, F, Fut>(
    config: &ImapConfig,
    endpoint: &SieveEndpoint,
    mut open: F,
) -> SieveResult<Session<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    F: FnMut() -> Fut,
    Fut: Future<Output = SieveResult<Session<S>>>,
{
    let origin = endpoint.origin();
    let request = engine_provider::CredentialRequest::new(CredentialProtocol::ManageSieve, &origin);
    engine_provider::with_credential(
        config.credentials.as_ref(),
        &request,
        BOUNDS.dial(),
        SieveError::is_sign_in_refusal,
        |credential| {
            let opening = open();
            async move {
                let mut session = opening.await?;
                session
                    .authenticate(&credential, &endpoint.server_name, endpoint.port())
                    .await?;
                Ok(session)
            }
        },
    )
    .await
}

/// Connects, reads the greeting, upgrades with `STARTTLS`, and reads what the server offers over
/// TLS: everything before anyone says who they are.
///
/// # Errors
///
/// [`ImapError`] on a TCP, TLS or greeting failure, a bad server name, or a server that does not
/// offer `STARTTLS`.
pub(crate) async fn open_tls(
    endpoint: &SieveEndpoint,
    connector: &TlsConnector,
) -> SieveResult<Session<TlsStream<TcpStream>>> {
    let server_name = ServerName::try_from(endpoint.server_name.clone())
        .map_err(|_| ImapError::bad("the ManageSieve server name is not a valid TLS name"))?;
    let tcp = deadline::connect(&endpoint.addr).await?;
    let mut plain = Session::open(tcp).await?;
    plain.start_tls().await?;
    let tls = deadline::handshake(connector, server_name, plain.into_inner_stream()?).await?;
    Session::open(tls).await
}

#[cfg(test)]
#[path = "dial_tests.rs"]
mod tests;
