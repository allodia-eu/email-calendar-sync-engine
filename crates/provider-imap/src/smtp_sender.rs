//! The SMTP transport an IMAP provider submits over, resolved once at `connect` from the
//! configured [`SmtpSettings`]. Split from `crate::filing`, which runs the sends, to keep that
//! file under the size limit.

use std::sync::Arc;

use tokio_rustls::TlsConnector;

use crate::{
    config::{ImapConfig, SmtpSecurity, SmtpSettings, port_of},
    credentials::{CredentialSource, Credentials},
    smtp_auth::SmtpAuth,
};

/// The resolved SMTP transport a provider holds after `connect`: plaintext, implicit
/// TLS, or STARTTLS, the two TLS variants carrying the connector each fresh send re-dials
/// with and the source each send asks for its credential (submission opens a new
/// connection per send, so it presents a token that is valid when it sends).
pub(crate) enum SmtpSender {
    Plaintext {
        addr: String,
    },
    ImplicitTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        credentials: Arc<dyn CredentialSource>,
    },
    StartTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        credentials: Arc<dyn CredentialSource>,
    },
}

impl SmtpSender {
    /// The authentication this transport presents with `credentials`, or `None` for the
    /// unauthenticated plaintext MX. `host`/`port` name the **SMTP** server, not the IMAP
    /// one: a SASL `OAUTHBEARER` response describes the connection it rides on (RFC 7628
    /// §3.1), and submission is a different host and port from the mailbox it files into.
    pub(crate) fn auth<'a>(&'a self, credentials: &'a Credentials) -> Option<SmtpAuth<'a>> {
        match self {
            Self::Plaintext { .. } => None,
            Self::ImplicitTls {
                addr, server_name, ..
            }
            | Self::StartTls {
                addr, server_name, ..
            } => Some(SmtpAuth {
                credentials,
                host: server_name,
                port: port_of(addr),
            }),
        }
    }
}

/// Resolves configured [`SmtpSettings`] into the [`SmtpSender`] the provider holds,
/// capturing the TLS connector and credentials each future send re-dials with.
pub(crate) fn resolve_smtp(
    settings: &SmtpSettings,
    connector: &TlsConnector,
    config: &ImapConfig,
) -> SmtpSender {
    match &settings.security {
        SmtpSecurity::Plaintext => SmtpSender::Plaintext {
            addr: settings.addr.clone(),
        },
        SmtpSecurity::ImplicitTls { server_name } => SmtpSender::ImplicitTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            credentials: Arc::clone(&config.credentials),
        },
        SmtpSecurity::StartTls { server_name } => SmtpSender::StartTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            credentials: Arc::clone(&config.credentials),
        },
    }
}
