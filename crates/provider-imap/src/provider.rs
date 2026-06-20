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
use engine_core::mail::{Mailbox, MailboxRole, Message};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_provider::{
    Capabilities, Draft, PageToken, Provider, ProviderError, ProviderResult, ScopeSync,
    SubmissionReceipt, SyncPage,
};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::error::{ImapError, ImapResult};
use crate::mail::{mailbox_from_list, message_key};
use crate::smtp::{self, Disposition};
use crate::sync::sync_page;
use crate::transport::Connection;

/// The IMAP folder list carries no sync token (a `LIST` re-snapshots it each pass),
/// so its cursor is a fixed sentinel — the store round-trips it unread.
const FOLDER_LIST_CURSOR: &str = "imap-folders";

/// SMTP submission settings captured at config time: the address, and — for a
/// real provider — the TLS server name that switches on implicit TLS + `AUTH
/// PLAIN`. `tls_server_name` is `None` for the Stalwart fixture's plaintext MX
/// (port 25, no auth) and `Some` for implicit TLS (port 465).
#[derive(Clone)]
struct SmtpSettings {
    addr: String,
    tls_server_name: Option<String>,
}

/// How to connect an [`ImapProvider`]: the address, the TLS server name, and
/// credentials. `Debug` redacts the password (`north-star.md` security).
#[derive(Clone)]
pub struct ImapConfig {
    addr: String,
    server_name: String,
    username: String,
    password: String,
    smtp: Option<SmtpSettings>,
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
        self.smtp = Some(SmtpSettings {
            addr: smtp_addr.into(),
            tls_server_name: None,
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
        self.smtp = Some(SmtpSettings {
            addr: smtp_addr.into(),
            tls_server_name: Some(server_name.into()),
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

/// Where a placed copy is filed. One value ties together the SPECIAL-USE role used
/// to resolve the server's real folder, the conventional folder name to fall back
/// to, and the fallback key prefix — so the three can never desync.
#[derive(Clone, Copy)]
enum Filing {
    Sent,
    Drafts,
}

impl Filing {
    /// The RFC 6154 SPECIAL-USE role identifying this folder on the server.
    fn role(self) -> MailboxRole {
        match self {
            Self::Sent => MailboxRole::Sent,
            Self::Drafts => MailboxRole::Drafts,
        }
    }

    /// The conventional folder name to create and use when the server advertises no
    /// folder with [`Self::role`].
    fn default_folder(self) -> &'static str {
        match self {
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
        }
    }

    /// The prefix of the `Message-ID`-derived fallback key (when no UIDPLUS).
    fn key_prefix(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Drafts => "draft",
        }
    }

    /// The IMAP flags to set on the appended copy.
    fn flags(self) -> &'static str {
        match self {
            Self::Sent => "\\Seen",
            Self::Drafts => "\\Draft \\Seen",
        }
    }
}

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
        // Resolve the SMTP sender first (cloning the connector), so SMTP-over-TLS can
        // re-dial with the host's trust policy after the IMAP connect consumes it.
        let smtp = config
            .smtp
            .as_ref()
            .map(|settings| resolve_smtp(settings, &connector, config));
        let tcp = TcpStream::connect(&config.addr).await?;
        let server_name = ServerName::try_from(config.server_name.clone())
            .map_err(|e| ImapError::bad(format!("invalid TLS server name: {e}")))?;
        let tls = connector.connect(server_name, tcp).await?;
        let mut connection = Connection::open(tls).await?;
        connection.login(&config.username, &config.password).await?;
        Ok(Self::build(connection, mailbox, smtp))
    }
}

/// Resolves configured [`SmtpSettings`] into the [`SmtpSender`] the provider holds,
/// capturing the TLS connector and credentials each future send re-dials with.
fn resolve_smtp(
    settings: &SmtpSettings,
    connector: &TlsConnector,
    config: &ImapConfig,
) -> SmtpSender {
    match &settings.tls_server_name {
        None => SmtpSender::Plaintext {
            addr: settings.addr.clone(),
        },
        Some(server_name) => SmtpSender::ImplicitTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
        },
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
        let message = smtp::assemble_message(draft, OffsetDateTime::now_utc())?;
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

        // Best-effort Sent placement; a successful send is never failed for it. The
        // Sent folder is resolved by its `\Sent` SPECIAL-USE role (falling back to
        // the conventional "Sent"), so the copy lands in the account's real Sent
        // folder — not a stray one on servers that name it differently.
        let (folder, append_uid) = self
            .append_to_role_folder(Filing::Sent, &message)
            .await
            .unwrap_or_else(|_| (Filing::Sent.default_folder().to_owned(), None));
        let email_key = placed_key(
            &folder,
            Filing::Sent.key_prefix(),
            append_uid,
            &draft.message_id,
        );
        Ok(SubmissionReceipt::new(email_key, draft.message_id.clone()))
    }

    /// Resolves the real folder for `filing` — the account's folder carrying the
    /// matching SPECIAL-USE role, else the conventional name (created if missing) —
    /// and APPENDs `message` flagged per `filing`, returning the folder used and the
    /// UIDPLUS `APPENDUID` if the server supports it.
    async fn append_to_role_folder(
        &self,
        filing: Filing,
        message: &[u8],
    ) -> ProviderResult<(String, Option<(u32, u32)>)> {
        let mut connection = self.connection.lock().await;
        let folder = if let Some(name) = resolve_role_folder(&mut connection, filing.role()).await?
        {
            name
        } else {
            // No folder advertises the role: fall back to the conventional name,
            // creating it (an "already exists" rejection is ignored).
            let name = filing.default_folder().to_owned();
            let _ = connection.create(&name).await;
            name
        };
        let append_uid = connection.append(&folder, filing.flags(), message).await?;
        Ok((folder, append_uid))
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
        let message = smtp::assemble_message(draft, OffsetDateTime::now_utc())?;
        // Unlike Sent placement this surfaces an `APPEND` failure (saving the draft is
        // the whole op). The Drafts folder is resolved by its `\Drafts` SPECIAL-USE
        // role (falling back to the conventional "Drafts").
        let (folder, append_uid) = self.append_to_role_folder(Filing::Drafts, &message).await?;
        Ok(placed_key(
            &folder,
            Filing::Drafts.key_prefix(),
            append_uid,
            &draft.message_id,
        ))
    }
}

/// Finds the account's folder carrying `role` (RFC 6154 SPECIAL-USE) via `LIST`;
/// `None` when the server advertises none.
async fn resolve_role_folder<S>(
    connection: &mut Connection<S>,
    role: MailboxRole,
) -> ImapResult<Option<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let rows = connection.list().await?;
    Ok(rows
        .iter()
        .filter_map(mailbox_from_list)
        .find(|mailbox| mailbox.role.as_ref() == Some(&role))
        .map(|mailbox| mailbox.name))
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
