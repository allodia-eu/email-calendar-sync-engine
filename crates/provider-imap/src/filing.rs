//! SMTP submission + IMAP `APPEND` filing of sent copies and drafts.
//!
//! The submission *conversation* lives in [`crate::smtp`]; this module is the
//! `Provider`-side glue that runs it and files the resulting copy into the account's
//! real Sent/Drafts folder (resolved by SPECIAL-USE role, `imap-smtp.md`). It is the
//! [`ImapProvider`] half that `submit_email` delegates to, kept out of
//! [`crate::provider`] so that file stays under the size limit.

use std::collections::HashSet;

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::MailboxRole,
};
use engine_provider::{Draft, ProviderError, ProviderResult, SubmissionReceipt};
use engine_rfc5322::{assemble_filed_message, assemble_message};
use time::OffsetDateTime;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::{ImapConfig, SmtpSecurity, SmtpSettings},
    error::{ImapError, ImapResult},
    mail::{mailbox_from_list, message_key},
    namespace::{MailStore, Namespaces},
    provider::ImapProvider,
    smtp::{self, Disposition, SmtpResult},
    transport::Connection,
};

/// The resolved SMTP transport a provider holds after `connect`: plaintext, implicit
/// TLS, or STARTTLS — the two TLS variants carrying the connector + credentials each
/// fresh send re-dials with (submission opens a new connection per send).
pub(crate) enum SmtpSender {
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
    StartTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        username: String,
        password: String,
    },
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
            username: config.username.clone(),
            password: config.password.clone(),
        },
        SmtpSecurity::StartTls { server_name } => SmtpSender::StartTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            username: config.username.clone(),
            password: config.password.clone(),
        },
    }
}

/// Everything derived from a draft once, shared by the wire send and the filed copy —
/// so a STARTTLS send (which negotiates before transmitting) and a plaintext/implicit
/// one run identical preparation and filing around the differing transmit step.
struct Submission {
    /// One timestamp for both the transmitted and filed copy (they differ only in Bcc).
    now: OffsetDateTime,
    /// The over-the-wire message — **without** the `Bcc` header.
    message: Vec<u8>,
    /// Envelope `MAIL FROM` address.
    from: String,
    /// De-duplicated envelope `RCPT TO` list (To + Cc + Bcc).
    to: Vec<String>,
    /// The `EHLO` identity (the sender's domain).
    ehlo: String,
}

/// Where a placed copy is filed. One value ties together the SPECIAL-USE role used
/// to resolve the server's real folder, the conventional folder name to fall back
/// to, and the fallback key prefix — so the three can never desync.
#[derive(Clone, Copy)]
pub(crate) enum Filing {
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

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImapProvider<S> {
    /// Submits `draft` over the provider's configured SMTP transport, opening a fresh
    /// connection per send. Plaintext and implicit TLS transmit directly; STARTTLS
    /// negotiates the cleartext upgrade, TLS-wraps the socket, then transmits over TLS.
    /// `AUTH PLAIN` runs only over an established TLS stream (implicit or post-upgrade).
    ///
    /// # Errors
    ///
    /// [`ProviderError::invalid_state`] when no SMTP transport is configured, or a
    /// classified failure on a rejected/ambiguous send or a transport error.
    pub(crate) async fn submit(&self, draft: &Draft) -> ProviderResult<SubmissionReceipt> {
        let sender = self
            .smtp
            .as_ref()
            .ok_or_else(|| ProviderError::invalid_state("no SMTP transport configured"))?;
        match sender {
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
                let tls = tls_connect(connector, server_name, tcp).await?;
                self.submit_over(tls, draft, Some((username, password)))
                    .await
            }
            SmtpSender::StartTls {
                addr,
                server_name,
                connector,
                username,
                password,
            } => {
                let sub = Self::prepare(draft)?;
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                // Cleartext STARTTLS handshake, then upgrade the socket and transmit
                // (with `AUTH PLAIN`) over the now-established TLS.
                let tcp = smtp::negotiate_starttls(tcp, &sub.ehlo).await?;
                let tls = tls_connect(connector, server_name, tcp).await?;
                let result = smtp::send_after_starttls(
                    tls,
                    &sub.ehlo,
                    &sub.from,
                    &sub.to,
                    &sub.message,
                    Some((username, password)),
                )
                .await?;
                self.file_result(result, &sub, draft).await
            }
        }
    }

    /// The submission core over an arbitrary SMTP stream — the seam the offline tests
    /// drive with a mock. Reads the greeting itself, so it is the plaintext / implicit-
    /// TLS path (STARTTLS reads the greeting during its negotiation and uses
    /// [`smtp::send_after_starttls`] via [`submit`](Self::submit)).
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] on a rejected/ambiguous send or assembly error.
    pub(crate) async fn submit_over<W>(
        &self,
        smtp: W,
        draft: &Draft,
        auth: Option<(&str, &str)>,
    ) -> ProviderResult<SubmissionReceipt>
    where
        W: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let sub = Self::prepare(draft)?;
        let result = smtp::send(smtp, &sub.ehlo, &sub.from, &sub.to, &sub.message, auth).await?;
        self.file_result(result, &sub, draft).await
    }

    /// Derives the [`Submission`] (wire message, envelope, EHLO identity) from `draft`.
    fn prepare(draft: &Draft) -> ProviderResult<Submission> {
        // One timestamp for both the transmitted and the filed copy, so they differ ONLY in
        // the Bcc header.
        let now = OffsetDateTime::now_utc();
        // The over-the-wire message OMITS the Bcc header — Bcc recipients are reached via the
        // envelope only, so no recipient can see them.
        let message = assemble_message(draft, now)?;
        let from = draft.from.email.as_str();
        // Every envelope recipient gets a `RCPT TO`: To + Cc + Bcc, de-duplicated
        // case-insensitively (the same address can appear in more than one field — e.g. To and
        // Cc) so a strict server never rejects a repeated `RCPT`. Bcc is delivered here but not
        // in the wire message's headers, so it stays hidden from the other recipients.
        let mut seen: HashSet<String> = HashSet::new();
        let to: Vec<String> = draft
            .to
            .iter()
            .chain(&draft.cc)
            .chain(&draft.bcc)
            .filter(|address| seen.insert(address.email.to_ascii_lowercase()))
            .map(|address| address.email.clone())
            .collect();
        let ehlo = from
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
            .to_owned();
        Ok(Submission {
            now,
            message,
            from: from.to_owned(),
            to,
            ehlo,
        })
    }

    /// Classifies the send's disposition, then (on delivery) files the Sent copy and
    /// returns its receipt.
    async fn file_result(
        &self,
        result: SmtpResult,
        sub: &Submission,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
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

        // The filed Sent copy INCLUDES the Bcc header (it is APPENDed locally, never
        // transmitted), so the sender's Sent folder records whom they Bcc'd — Outlook/
        // Thunderbird behavior. Identical to the wire message when there's no Bcc, so only
        // re-assemble then.
        let filed = if draft.bcc.is_empty() {
            sub.message.clone()
        } else {
            assemble_filed_message(draft, sub.now)?
        };
        // Best-effort Sent placement; a successful send is never failed for it. The
        // Sent folder is resolved by its `\Sent` SPECIAL-USE role (falling back to
        // the conventional "Sent"), so the copy lands in the account's real Sent
        // folder — not a stray one on servers that name it differently.
        let (folder, append_uid) = self
            .append_to_role_folder(Filing::Sent, &filed)
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
        let folder = if let Some(name) = resolve_role_folder(
            &mut connection,
            &self.namespaces,
            &self.store,
            filing.role(),
        )
        .await?
        {
            name
        } else {
            // No folder advertises the role: fall back to the conventional name,
            // creating it (an "already exists" rejection is ignored). Qualified by the
            // bound store, so a shared mailbox with no `\Sent` gets one of its own rather
            // than having its sent copy filed into the credential's personal Sent folder.
            let name = self.store.qualify(filing.default_folder());
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
        // A saved draft retains the Bcc header so resuming it restores every recipient (it is
        // APPENDed locally, never transmitted).
        let message = assemble_filed_message(draft, OffsetDateTime::now_utc())?;
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

/// TLS-wraps `tcp` with `connector`, presenting `server_name` (SNI/cert name; may
/// differ from a loopback address). Shared by the implicit-TLS and post-STARTTLS
/// submission paths.
async fn tls_connect(
    connector: &TlsConnector,
    server_name: &str,
    tcp: TcpStream,
) -> ProviderResult<TlsStream<TcpStream>> {
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| ImapError::bad(format!("invalid SMTP TLS server name: {e}")))?;
    let tls = connector
        .connect(name, tcp)
        .await
        .map_err(ImapError::from)?;
    Ok(tls)
}

/// Finds the folder carrying `role` (RFC 6154 SPECIAL-USE) in the store this provider is
/// bound to; `None` when it advertises none.
///
/// Scoped to the store, not to everything `LIST` returns, and that matters concretely: a
/// shared mailbox has its own `\Sent` folder, and a flat `LIST` interleaves it with the
/// credential's. Picking the first match would file one principal's sent copy into another
/// principal's folder — and `Shared Folders/...` sorts after a bare `Sent Items` on
/// Stalwart, so it would have looked correct there while being wrong on any server that
/// orders differently (`crate::discovery`).
async fn resolve_role_folder<S>(
    connection: &mut Connection<S>,
    namespaces: &Namespaces,
    store: &MailStore,
    role: MailboxRole,
) -> ImapResult<Option<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let rows = connection.list_pattern(&store.list_pattern()).await?;
    Ok(rows
        .iter()
        .filter(|row| store.contains(namespaces, &row.name))
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
#[path = "filing_tests.rs"]
mod tests;

// The submission-dispatch tests drive a real in-process SMTP server (their own cert +
// TLS harness), so they live in a sibling file to keep `filing_tests.rs` small.
#[cfg(test)]
#[path = "filing_smtp_server_tests.rs"]
mod smtp_server_tests;
