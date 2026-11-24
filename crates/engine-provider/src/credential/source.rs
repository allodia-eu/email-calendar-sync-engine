//! [`CredentialSource`], the request it answers, and [`FixedCredential`].

use core::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use super::Credential;
use crate::ProviderError;

/// Who a source signs in as: stable, not secret, and unique within the process. `Debug` does not
/// print the name.
///
/// Everything authenticated is keyed by it: an IMAP connection, a JMAP session, a request gate.
/// The host names it (a sync host its account id, a transfer host one side of a mailbox pair, a
/// direct host its tenant and the user's OIDC subject). It is its own type rather than an
/// account id because a host serving many tenants may reuse account ids across them.
///
/// A subject [`FixedCredential::unique`] mints never equals one a host names, whatever the
/// name, so two fixed credentials never share anything by accident.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialSubject(Subject);

#[derive(Clone, PartialEq, Eq, Hash)]
enum Subject {
    Named(Box<str>),
    Unique(u64),
}

impl CredentialSubject {
    /// The subject the host calls `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(Subject::Named(name.into().into_boxed_str()))
    }

    /// A subject no other call returns, and that no host-named subject equals.
    pub(crate) fn unique() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(Subject::Unique(NEXT.fetch_add(1, Ordering::Relaxed)))
    }
}

/// Says whether the subject is named by the host or minted, and nothing else. A host's name
/// is often personal (an account address, one side of a mailbox pair, a tenant and the user's
/// OIDC subject), so it stays out of a `{:?}` for the reason a credential's username does.
impl core::fmt::Debug for CredentialSubject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match &self.0 {
            Subject::Named(_) => "<named>",
            Subject::Unique(_) => "<minted>",
        };
        write!(f, "CredentialSubject({kind})")
    }
}

/// Where a credential is about to go: a scheme, a host and a port.
///
/// The host is the name the TLS handshake verifies, not the address dialled, so a loopback
/// fixture mapped to a real name is still that name. The port is the effective one, the
/// scheme's default when the configuration gave none.
///
/// The scheme says how the connection is secured as well as what it speaks, so a source can
/// tell a nonstandard port or plaintext from the rest. An HTTP origin's scheme is `https` or
/// `http`. IMAP and SMTP use the IANA service names of RFC 8314 §7: `imaps` (TLS from the first
/// byte) and `imap` (`STARTTLS`), `submissions` and `submission` (`STARTTLS`). ManageSieve uses
/// `sieve`, its URI scheme (RFC 5804 §3), whose only transport is `STARTTLS`. None of them ever
/// presents a credential before TLS is up.
///
/// Scheme and host are kept in ASCII lowercase, so origins compare as the URL standard
/// compares them. An IPv6 address is kept without the brackets a URL puts around it (`::1`,
/// not `[::1]`), the form IMAP's TLS server name takes, so an HTTP origin and an IMAP one
/// name the same host the same way.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Origin {
    scheme: Box<str>,
    host: Box<str>,
    port: u16,
}

impl Origin {
    /// The origin `scheme://host:port`, with scheme and host lowercased, and an IPv6 host's
    /// URL brackets removed.
    #[must_use]
    pub fn new(scheme: &str, host: &str, port: u16) -> Self {
        let host = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        Self {
            scheme: scheme.to_ascii_lowercase().into_boxed_str(),
            host: host.to_ascii_lowercase().into_boxed_str(),
            port,
        }
    }

    /// The scheme, lowercase.
    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// The host, lowercase.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The effective port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// The protocol a credential is about to be presented over.
///
/// Named as well as the [`Origin`], so a host whose account has separate credentials for
/// submission need not infer the protocol from a port number. Most sources ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum CredentialProtocol {
    /// IMAP: `LOGIN` or `AUTHENTICATE`.
    Imap,
    /// SMTP submission: `AUTH`.
    Smtp,
    /// ManageSieve (RFC 5804), where an IMAP account's mail rules and out-of-office reply are
    /// kept: `AUTHENTICATE`.
    ManageSieve,
    /// An HTTP request's `Authorization` header: JMAP, CalDAV, CardDAV, Graph, Google.
    Http,
}

/// What a source is told about the credential it is asked for: the protocol it will be
/// presented over, and the origin it will go to.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct CredentialRequest<'a> {
    /// The protocol the credential is about to be presented over.
    pub protocol: CredentialProtocol,
    /// Where it is going.
    pub origin: &'a Origin,
}

impl<'a> CredentialRequest<'a> {
    /// A request for a credential to present over `protocol` to `origin`.
    #[must_use]
    pub fn new(protocol: CredentialProtocol, origin: &'a Origin) -> Self {
        Self { protocol, origin }
    }
}

/// Where an adapter gets the credential it presents, each time it presents one.
///
/// **Account-scoped.** A source answers for one [`subject`](Self::subject). An adapter is bound
/// to one source for its whole life, and nothing authenticated is ever shared across subjects.
///
/// **Errors are classified as the host sees them**, and an adapter passes the class through
/// unchanged: a revoked grant, or a broker with no consent for the mailbox, is
/// [`Authentication`]; an unreachable token endpoint or broker is [`Retryable`]; a `429` from
/// the token endpoint is [`RateLimited`]; no credential for this origin is [`Permanent`].
///
/// [`Authentication`]: engine_core::error::FailureClass::Authentication
/// [`Retryable`]: engine_core::error::FailureClass::Retryable
/// [`RateLimited`]: engine_core::error::FailureClass::RateLimited
/// [`Permanent`]: engine_core::error::FailureClass::Permanent
#[async_trait]
pub trait CredentialSource: Send + Sync + core::fmt::Debug {
    /// Who this source signs in as. Everything authenticated is keyed by it.
    fn subject(&self) -> &CredentialSubject;

    /// Whether the credential may go to `origin`. Asked before anything is sent to it; a
    /// refusal fails the operation as [`Permanent`] and sends nothing. The default permits
    /// every origin.
    ///
    /// [`Permanent`]: engine_core::error::FailureClass::Permanent
    fn permits(&self, origin: &Origin) -> bool {
        let _ = origin;
        true
    }

    /// The credential to present now. A token source returns one valid now, refreshing first
    /// if it is near expiry. Called concurrently, so it must be cheap when cached and coalesce
    /// refreshes.
    ///
    /// The adapter waits for the answer no longer than the step it belongs to may take (the
    /// dial on IMAP and SMTP), and fails as retryable past that.
    ///
    /// # Errors
    ///
    /// Whatever stops the source producing one, classified as described on the trait.
    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError>;

    /// A replacement for `refused`, which the server has just refused, or `None`. Asked at most
    /// once per dial, submission or request.
    ///
    /// The default is `None`: the same password gets the same answer, at a provider that may
    /// be counting failures towards a lockout. A token source refreshes here even when the
    /// token has not reached its stated expiry, because the server has just said it is no
    /// longer good; callers refusing the same token should share one refresh.
    ///
    /// # Errors
    ///
    /// As [`credential`](Self::credential).
    async fn replace(
        &self,
        refused: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        let _ = (refused, request);
        Ok(None)
    }
}

/// A credential that never changes, as a source: the same answer every time, and never a
/// replacement.
#[derive(Clone, Debug)]
pub struct FixedCredential {
    subject: CredentialSubject,
    credential: Credential,
}

impl FixedCredential {
    /// `credential`, signing in as `subject`.
    #[must_use]
    pub fn new(subject: CredentialSubject, credential: Credential) -> Self {
        Self {
            subject,
            credential,
        }
    }

    /// `credential`, under a subject minted for it that no other source shares, so two fixed
    /// credentials never share a connection pool or a session by accident.
    #[must_use]
    pub fn unique(credential: Credential) -> Self {
        Self::new(CredentialSubject::unique(), credential)
    }
}

#[async_trait]
impl CredentialSource for FixedCredential {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(self.credential.clone())
    }
}
