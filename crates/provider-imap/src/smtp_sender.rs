//! The SMTP transport an IMAP provider submits over, resolved once at `connect` from the
//! configured [`SmtpSettings`]. Split from `crate::filing`, which runs the sends, to keep that
//! file under the size limit.

use std::sync::Arc;

use engine_provider::{Credential, CredentialSource, Origin};
use tokio_rustls::TlsConnector;

use crate::{
    config::{ImapConfig, SmtpSecurity, SmtpSettings, port_of},
    smtp_auth::SmtpAuth,
};

/// The resolved SMTP transport a provider holds after `connect`: plaintext, implicit
/// TLS, or STARTTLS, the two TLS variants carrying the connector each fresh send re-dials
/// with, the source each send asks for its credential (submission opens a new
/// connection per send, so it presents a token that is valid when it sends), and the
/// [`Origin`] the source is told the credential goes to.
pub(crate) enum SmtpSender {
    Plaintext {
        addr: String,
    },
    ImplicitTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        credentials: Arc<dyn CredentialSource>,
        origin: Origin,
    },
    StartTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        credentials: Arc<dyn CredentialSource>,
        origin: Origin,
    },
}

impl SmtpSender {
    /// The authentication this transport presents with `credentials`, or `None` for the
    /// unauthenticated plaintext MX. `host`/`port` name the **SMTP** server, not the IMAP
    /// one: a SASL `OAUTHBEARER` response describes the connection it rides on (RFC 7628
    /// §3.1), and submission is a different host and port from the mailbox it files into.
    pub(crate) fn auth<'a>(&'a self, credential: &'a Credential) -> Option<SmtpAuth<'a>> {
        match self {
            Self::Plaintext { .. } => None,
            Self::ImplicitTls {
                addr, server_name, ..
            }
            | Self::StartTls {
                addr, server_name, ..
            } => Some(SmtpAuth {
                credential,
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
            origin: origin("submissions", 465, &settings.addr, server_name),
        },
        SmtpSecurity::StartTls { server_name } => SmtpSender::StartTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            credentials: Arc::clone(&config.credentials),
            origin: origin("submission", 587, &settings.addr, server_name),
        },
    }
}

/// Where a submission's `AUTH` goes, as the source is told: the TLS server name, the port
/// (`default_port` when the address has none), and `scheme`: `submissions`, or `submission`
/// for `STARTTLS`, RFC 8314 §7's service names. The plaintext MX is sent no credential and has
/// no origin.
fn origin(scheme: &str, default_port: u16, addr: &str, server_name: &str) -> Origin {
    Origin::new(scheme, server_name, port_of(addr).unwrap_or(default_port))
}
