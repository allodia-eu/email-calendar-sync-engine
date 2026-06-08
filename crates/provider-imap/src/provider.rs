//! The [`Provider`] implementation: an [`ImapProvider`] bound to one mailbox for
//! email, syncing the account's folder list under the per-account
//! [`SyncScope::ImapMailboxList`].
//!
//! The connection is stateful (one TLS socket, sequential commands), so it is held
//! behind an async [`Mutex`] — concurrent `sync_email_page` calls serialize onto
//! the one IMAP session, which is exactly IMAP's model. Method execution is generic
//! over the stream, so the offline tests drive the full `Provider` surface over a
//! mock while [`ImapProvider::connect`] uses a `tokio-rustls` TLS stream.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey};
use engine_core::mail::{Mailbox, Message};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_provider::{
    Capabilities, Draft, PageToken, Provider, ProviderError, ProviderResult, ScopeSync,
    SubmissionReceipt, SyncPage,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::error::ImapError;
use crate::mail::{mailbox_from_list, message_key};
use crate::smtp::{self, Disposition};
use crate::sync::sync_page;
use crate::transport::Connection;

/// The IMAP folder list carries no sync token (a `LIST` re-snapshots it each pass),
/// so its cursor is a fixed sentinel — the store round-trips it unread.
const FOLDER_LIST_CURSOR: &str = "imap-folders";

/// How SMTP submission connects. The Stalwart fixture uses plaintext with no auth
/// (an MX on port 25); a real provider uses implicit TLS with `AUTH PLAIN`.
#[derive(Clone)]
enum SmtpConfig {
    Plaintext { addr: String },
    ImplicitTls { addr: String, server_name: String },
}

/// How to connect an [`ImapProvider`]: the address, the TLS server name, and
/// credentials. `Debug` redacts the password (`north-star.md` security).
#[derive(Clone)]
pub struct ImapConfig {
    addr: String,
    server_name: String,
    username: String,
    password: String,
    smtp: Option<SmtpConfig>,
}

impl ImapConfig {
    /// Configures an implicit-TLS IMAP connection to `addr` (`host:port`),
    /// presenting `server_name` for TLS (SNI/cert name; may differ from a loopback
    /// `addr`) and authenticating as `username`/`password`.
    #[must_use]
    pub fn new(
        addr: impl Into<String>,
        server_name: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            addr: addr.into(),
            server_name: server_name.into(),
            username: username.into(),
            password: password.into(),
            smtp: None,
        }
    }

    /// Enables **plaintext** SMTP submission via `smtp_addr` (`host:port`), with no
    /// authentication — for an MX that accepts local mail (the Stalwart fixture's
    /// port 25). Without any SMTP config the provider advertises no submission
    /// capability and [`submit_email`](Provider::submit_email) is rejected.
    #[must_use]
    pub fn with_smtp(mut self, smtp_addr: impl Into<String>) -> Self {
        self.smtp = Some(SmtpConfig::Plaintext {
            addr: smtp_addr.into(),
        });
        self
    }

    /// Enables **implicit-TLS** SMTP submission via `smtp_addr` (`host:port`,
    /// typically `:465`), authenticating with `AUTH PLAIN` using the account
    /// credentials. The injected TLS connector (from [`ImapProvider::connect`])
    /// secures the connection, presenting `server_name`. STARTTLS (port 587) is a
    /// later refinement.
    #[must_use]
    pub fn with_smtp_tls(
        mut self,
        smtp_addr: impl Into<String>,
        server_name: impl Into<String>,
    ) -> Self {
        self.smtp = Some(SmtpConfig::ImplicitTls {
            addr: smtp_addr.into(),
            server_name: server_name.into(),
        });
        self
    }
}

impl core::fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapConfig")
            .field("addr", &self.addr)
            .field("server_name", &self.server_name)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// The folder a sent copy is filed into.
const SENT_MAILBOX: &str = "Sent";

/// The folder a saved draft is filed into.
const DRAFTS_MAILBOX: &str = "Drafts";

/// The resolved SMTP transport a provider holds after `connect`: plaintext, or
/// implicit TLS carrying the connector + credentials each fresh send re-dials with.
enum SmtpSender {
    Plaintext {
        addr: String,
    },
    ImplicitTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        username: String,
        password: String,
    },
}

/// An IMAP read/sync provider bound to a single mailbox for its email scope, with
/// optional SMTP submission.
pub struct ImapProvider<S> {
    connection: Mutex<Connection<S>>,
    mailbox: MailboxId,
    smtp: Option<SmtpSender>,
    capabilities: Capabilities,
}

impl<S> core::fmt::Debug for ImapProvider<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapProvider")
            .field("mailbox", &self.mailbox)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl ImapProvider<TlsStream<TcpStream>> {
    /// Connects over implicit TLS, logs in, and binds `mailbox` for the email scope.
    ///
    /// The `connector` carries the host's trust policy — the library never bakes in
    /// a root store, so a mobile host (or the self-signed test fixture) injects its
    /// own (`docs/agent-guidance/imap-smtp.md`).
    ///
    /// # Errors
    ///
    /// [`ImapError`] on a TCP/TLS/login failure or a bad server name.
    pub async fn connect(
        config: &ImapConfig,
        connector: TlsConnector,
        mailbox: MailboxId,
    ) -> Result<Self, ImapError> {
        // Clone the connector before the IMAP connect consumes it, so SMTP-over-TLS
        // can re-dial with the host's trust policy.
        let smtp = config
            .smtp
            .as_ref()
            .map(|smtp| smtp.resolve(connector.clone(), config));
        let tcp = TcpStream::connect(&config.addr).await?;
        let server_name = ServerName::try_from(config.server_name.clone())
            .map_err(|e| ImapError::bad(format!("invalid TLS server name: {e}")))?;
        let tls = connector.connect(server_name, tcp).await?;
        let mut connection = Connection::open(tls).await?;
        connection.login(&config.username, &config.password).await?;
        Ok(Self::build(connection, mailbox, smtp))
    }
}

impl SmtpConfig {
    /// Resolves a configured transport into the sender the provider holds, carrying
    /// the TLS connector and credentials needed for each future send.
    fn resolve(&self, connector: TlsConnector, config: &ImapConfig) -> SmtpSender {
        match self {
            Self::Plaintext { addr } => SmtpSender::Plaintext { addr: addr.clone() },
            Self::ImplicitTls { addr, server_name } => SmtpSender::ImplicitTls {
                addr: addr.clone(),
                server_name: server_name.clone(),
                connector,
                username: config.username.clone(),
                password: config.password.clone(),
            },
        }
    }
}

impl<S> ImapProvider<S> {
    /// Builds a provider, advertising submission iff SMTP is configured.
    fn build(connection: Connection<S>, mailbox: MailboxId, smtp: Option<SmtpSender>) -> Self {
        let mut capabilities = Capabilities::none().with_mail();
        if smtp.is_some() {
            capabilities = capabilities.with_submission();
        }
        Self {
            connection: Mutex::new(connection),
            mailbox,
            smtp,
            capabilities,
        }
    }

    /// Wraps an already-open, logged-in connection bound to `mailbox` (mail only).
    /// Offline tests use this over a mock stream; the live path is
    /// [`ImapProvider::connect`].
    #[cfg(test)]
    pub(crate) fn with_connection(connection: Connection<S>, mailbox: MailboxId) -> Self {
        Self::build(connection, mailbox, None)
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> Provider for ImapProvider<S> {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// IMAP folder-list state is per account, so the mailbox container syncs under
    /// [`SyncScope::ImapMailboxList`] — distinct from any one mailbox's email scope.
    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailboxList {
            account: account.clone(),
        }
    }

    /// IMAP email state is per mailbox, so this provider's email scope names its
    /// bound mailbox.
    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let rows = {
            let mut connection = self.connection.lock().await;
            connection.list().await?
        };
        let mailboxes: Vec<Mailbox> = rows.iter().filter_map(mailbox_from_list).collect();
        // `LIST` is a full snapshot every pass, so every folder is `present`.
        let present: BTreeSet<ProviderKey> = mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(mailboxes, present),
            SyncState::new(FOLDER_LIST_CURSOR),
        ))
    }

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        let mut connection = self.connection.lock().await;
        Ok(sync_page(&mut connection, &self.mailbox, cursor, page, limit).await?)
    }

    /// Submits `draft` over SMTP and files the sent copy in Sent.
    ///
    /// The pre-generated `Message-ID` travels on the message, so the sent copy
    /// reconciles by it. A post-`DATA` ambiguity becomes a
    /// [`ProviderError::needs_confirmation`] (never blind-retried); a clean
    /// rejection is permanent (5xx) or transient (4xx). Sent placement is a
    /// best-effort `APPEND` — a successful send is not failed for a Sent-filing
    /// hiccup; with UIDPLUS the receipt carries the real Sent key, otherwise a
    /// `Message-ID`-derived one that the next Sent sync resolves.
    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        let smtp = self
            .smtp
            .as_ref()
            .ok_or_else(|| ProviderError::invalid_state("no SMTP transport configured"))?;
        match smtp {
            SmtpSender::Plaintext { addr } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                self.submit_over(tcp, draft, None).await
            }
            SmtpSender::ImplicitTls {
                addr,
                server_name,
                connector,
                username,
                password,
            } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                let name = ServerName::try_from(server_name.clone())
                    .map_err(|e| ImapError::bad(format!("invalid SMTP TLS server name: {e}")))?;
                let tls = connector
                    .connect(name, tcp)
                    .await
                    .map_err(ImapError::from)?;
                self.submit_over(tls, draft, Some((username.as_str(), password.as_str())))
                    .await
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImapProvider<S> {
    /// The submission core over an arbitrary SMTP stream — the seam the offline
    /// tests drive with a mock while [`Provider::submit_email`] supplies a TCP (or
    /// TLS) socket. Runs the conversation (optionally authenticating with `auth`),
    /// maps the disposition to a result/classified error, then files the Sent copy
    /// via the IMAP connection.
    pub(crate) async fn submit_over<W>(
        &self,
        smtp: W,
        draft: &Draft,
        auth: Option<(&str, &str)>,
    ) -> ProviderResult<SubmissionReceipt>
    where
        W: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let message = smtp::assemble_message(draft);
        let from = draft.from.email.as_str();
        let to: Vec<String> = draft
            .to
            .iter()
            .map(|address| address.email.clone())
            .collect();
        let ehlo = from
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain);

        let result = smtp::send(smtp, ehlo, from, &to, &message, auth).await?;
        match result.disposition {
            Disposition::Delivered => {}
            Disposition::RejectedPermanent(text) => {
                return Err(ProviderError::permanent(format!("SMTP rejected: {text}")));
            }
            Disposition::RejectedTransient(text) => {
                return Err(ProviderError::retryable(format!("SMTP deferred: {text}")));
            }
            Disposition::Ambiguous(text) => {
                return Err(ProviderError::needs_confirmation(format!(
                    "SMTP outcome ambiguous: {text}"
                )));
            }
        }

        // Best-effort Sent placement; a successful send is never failed for it.
        // Ensure the Sent folder exists first (the fixture has none until a client
        // creates it); an "already exists" rejection is ignored.
        let append_uid = {
            let mut connection = self.connection.lock().await;
            let _ = connection.create(SENT_MAILBOX).await;
            connection
                .append(SENT_MAILBOX, "\\Seen", &message)
                .await
                .ok()
                .flatten()
        };
        let email_key = placed_key(SENT_MAILBOX, "sent", append_uid, &draft.message_id);
        Ok(SubmissionReceipt::new(email_key, draft.message_id.clone()))
    }

    /// Saves `draft` as a message in the Drafts folder via IMAP `APPEND` — no SMTP,
    /// so it works against any IMAP server. Ensures Drafts exists (`CREATE`, ignoring
    /// "already exists"), appends the assembled RFC 5322 message flagged `\Draft`,
    /// and returns its key (the real Drafts key from UIDPLUS `APPENDUID`, or a
    /// `Message-ID`-derived key the next Drafts sync resolves).
    ///
    /// Unlike Sent placement this is **not** best-effort: a failed `APPEND` is
    /// surfaced, since saving the draft is the whole operation.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] on a transport or `APPEND` failure.
    pub async fn save_draft(&self, draft: &Draft) -> ProviderResult<ProviderKey> {
        let message = smtp::assemble_message(draft);
        let mut connection = self.connection.lock().await;
        let _ = connection.create(DRAFTS_MAILBOX).await;
        let append_uid = connection
            .append(DRAFTS_MAILBOX, "\\Draft \\Seen", &message)
            .await?;
        Ok(placed_key(
            DRAFTS_MAILBOX,
            "draft",
            append_uid,
            &draft.message_id,
        ))
    }
}

/// The key for a message just placed in `folder`: the real key from UIDPLUS
/// `APPENDUID`, else a `Message-ID`-derived `{prefix}:<id>` key the next sync of
/// that folder resolves.
fn placed_key(
    folder: &str,
    prefix: &str,
    append_uid: Option<(u32, u32)>,
    message_id: &MessageIdHeader,
) -> ProviderKey {
    match append_uid {
        Some((validity, uid)) => message_key(folder, validity, uid),
        None => ProviderKey::new(format!("{prefix}:{}", message_id.as_str()))
            .expect("a Message-ID-derived placement key is never empty"),
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
