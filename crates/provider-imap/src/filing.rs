//! SMTP submission + IMAP `APPEND` filing of sent copies and drafts.
//!
//! The submission *conversation* lives in [`crate::smtp`] and the `APPEND` itself in
//! [`crate::place`]; this module is the `Provider`-side glue that runs the send and then
//! files the resulting copy into the account's real Sent/Drafts folder. It is the
//! [`ImapProvider`] half that `submit_email` delegates to, kept out of
//! [`crate::provider`] so that file stays under the size limit.
//!
//! **Delivering and filing are two operations, and the second one can fail on its own.**
//! SMTP dials a fresh connection per send, so a delivery succeeds over a session that has
//! been idle for an hour; the `APPEND` rides the provider's standing IMAP session, which by
//! then may be dead. That asymmetry is not hypothetical: it delivers the mail and loses the
//! sender's copy, with nothing to reconcile against later. The response is `file_sent_copy`:
//! retry once on a
//! freshly dialed session, and when even that cannot file it, say so in the receipt
//! ([`engine_provider::SentCopy::Unfiled`]) — this crate emits no logs of its own, so the
//! outcome travelling up *is* the diagnostic. What it must never do is fail the send: the
//! mail is already gone, and a caller that treated filing as delivery would re-send it.

use std::collections::HashSet;

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, ProviderError, ProviderResult, SubmissionReceipt};
use engine_rfc5322::{assemble_filed_message, assemble_message};
use time::OffsetDateTime;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::{ImapConfig, SmtpSecurity, SmtpSettings, port_of},
    credentials::Credentials,
    dial::connect_session,
    error::ImapError,
    place::{Filing, append_to_role_folder, place_if_absent, placed_key},
    provider::ImapProvider,
    smtp::{self, Disposition, SmtpResult},
    smtp_auth::SmtpAuth,
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
        credentials: Credentials,
    },
    StartTls {
        addr: String,
        server_name: String,
        connector: TlsConnector,
        credentials: Credentials,
    },
}

impl SmtpSender {
    /// The authentication this transport presents, or `None` for the unauthenticated
    /// plaintext MX. `host`/`port` name the **SMTP** server, not the IMAP one: a SASL
    /// `OAUTHBEARER` response describes the connection it rides on (RFC 7628 §3.1), and
    /// submission is a different host and port from the mailbox it files into.
    fn auth(&self) -> Option<SmtpAuth<'_>> {
        match self {
            Self::Plaintext { .. } => None,
            Self::ImplicitTls {
                addr,
                server_name,
                credentials,
                ..
            }
            | Self::StartTls {
                addr,
                server_name,
                credentials,
                ..
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
            credentials: config.credentials.clone(),
        },
        SmtpSecurity::StartTls { server_name } => SmtpSender::StartTls {
            addr: settings.addr.clone(),
            server_name: server_name.clone(),
            connector: connector.clone(),
            credentials: config.credentials.clone(),
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

/// What a fresh IMAP session needs, so a failed placement can be retried on a new
/// connection instead of lost on a dead one.
///
/// Held only by a provider built by [`ImapProvider::connect`] — one built over a mock
/// stream has no server to re-dial, and says so by carrying `None`.
pub(crate) struct Redial {
    config: ImapConfig,
    connector: TlsConnector,
}

impl Redial {
    /// Captures what [`connect_session`] needs to open another session like this one.
    pub(crate) fn new(config: &ImapConfig, connector: &TlsConnector) -> Self {
        let mut config = config.clone();
        // A retry dial is not an account connecting: reporting `TlsEstablished` /
        // `Authenticated` through the host's observer would put a second "connected" pair
        // in its log for what is really one send finishing its filing.
        config.connect_observer = None;
        Self {
            config,
            connector: connector.clone(),
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
                ..
            } => {
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                let tls = tls_connect(connector, server_name, tcp).await?;
                self.submit_over(tls, draft, sender.auth()).await
            }
            SmtpSender::StartTls {
                addr,
                server_name,
                connector,
                ..
            } => {
                let sub = Self::prepare(draft)?;
                let tcp = TcpStream::connect(addr).await.map_err(ImapError::from)?;
                // Cleartext STARTTLS handshake, then upgrade the socket and transmit
                // (authenticating) over the now-established TLS.
                let tcp = smtp::negotiate_starttls(tcp, &sub.ehlo).await?;
                let tls = tls_connect(connector, server_name, tcp).await?;
                let result = smtp::send_after_starttls(
                    tls,
                    &sub.ehlo,
                    &sub.from,
                    &sub.to,
                    &sub.message,
                    sender.auth(),
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
        auth: Option<SmtpAuth<'_>>,
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
        // The Sent folder is resolved by its `\Sent` SPECIAL-USE role (falling back to
        // the conventional "Sent"), so the copy lands in the account's real Sent
        // folder — not a stray one on servers that name it differently.
        match self.file_sent_copy(&filed, draft).await {
            Ok((folder, append_uid)) => Ok(SubmissionReceipt::filed(
                placed_key(
                    &folder,
                    Filing::Sent.key_prefix(),
                    append_uid,
                    &draft.message_id,
                ),
                draft.message_id.clone(),
            )),
            // Delivered, but the copy is not in Sent and no later sync can find it — there
            // is nothing on the server to reconcile against. Never an `Err`: the message
            // has reached its recipients, and a caller that saw a failure here would
            // re-send it.
            Err(detail) => Ok(SubmissionReceipt::unfiled(
                placed_key(
                    Filing::Sent.default_folder(),
                    Filing::Sent.key_prefix(),
                    None,
                    &draft.message_id,
                ),
                draft.message_id.clone(),
                detail,
            )),
        }
    }

    /// Files the delivered message's Sent copy: on the provider's standing session, else
    /// on a freshly dialed one.
    ///
    /// The standing session is the one that goes stale — it may have sat unused since the
    /// last sync while SMTP dialed fresh — so its failure is the *expected* path, not an
    /// exceptional one. The retry re-dials and, before appending, asks whether the copy is
    /// already there: the first attempt may have committed server-side and lost only its
    /// response, and two copies in Sent is its own bug.
    ///
    /// # Errors
    ///
    /// The detail to record on the receipt when neither attempt filed it.
    async fn file_sent_copy(
        &self,
        filed: &[u8],
        draft: &Draft,
    ) -> Result<(String, Option<(u32, u32)>), String> {
        let first = {
            let mut connection = self.connection.lock().await;
            append_to_role_folder(&mut connection, Filing::Sent, filed).await
        };
        let first = match first {
            Ok(placed) => return Ok(placed),
            Err(err) => err,
        };
        let Some(redial) = self.redial.as_ref() else {
            return Err(format!("{first}"));
        };
        self.refile_on_a_fresh_session(redial, filed, draft)
            .await
            .map_err(|retry| format!("{first}; retry on a fresh session: {retry}"))
    }

    /// The retry: dial a new session, and file the copy there unless it is already filed.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] on a dial, `SEARCH` or `APPEND` failure.
    async fn refile_on_a_fresh_session(
        &self,
        redial: &Redial,
        filed: &[u8],
        draft: &Draft,
    ) -> ProviderResult<(String, Option<(u32, u32)>)> {
        let (mut connection, _) = connect_session(&redial.config, &redial.connector)
            .await
            .map_err(ProviderError::from)?;
        // `APPEND` is not idempotent, so the probe inside asks before placing: a first
        // attempt that committed and lost its response must not become two copies.
        place_if_absent(&mut connection, Filing::Sent, &draft.message_id, filed).await
    }

    /// Files the Sent copy of a message that **has already been delivered** — the repair a
    /// host runs when a submission came back
    /// [`SentCopy::Unfiled`](engine_provider::SentCopy::Unfiled) and the user asked to try
    /// again.
    ///
    /// Sends nothing, and is idempotent at every step: **both** attempts probe for the copy
    /// before placing it, so running this ten times leaves exactly one copy in Sent. That
    /// matters more here than on the send path — a repair the user can press repeatedly is
    /// one they *will* press repeatedly.
    ///
    /// # Errors
    ///
    /// A classified [`ProviderError`] if neither the standing session nor a freshly dialed
    /// one could file it. The caller may offer the retry again.
    pub(crate) async fn refile(&self, draft: &Draft) -> ProviderResult<ProviderKey> {
        // The filed copy keeps the Bcc header, exactly as the original filing would have.
        let filed = assemble_filed_message(draft, OffsetDateTime::now_utc())?;
        let first = {
            let mut connection = self.connection.lock().await;
            place_if_absent(&mut connection, Filing::Sent, &draft.message_id, &filed).await
        };
        let (folder, append_uid) = match first {
            Ok(placed) => placed,
            Err(standing) => match self.redial.as_ref() {
                Some(redial) => {
                    self.refile_on_a_fresh_session(redial, &filed, draft)
                        .await?
                }
                None => return Err(standing),
            },
        };
        Ok(placed_key(
            &folder,
            Filing::Sent.key_prefix(),
            append_uid,
            &draft.message_id,
        ))
    }

    /// Saves `draft` as a message in the Drafts folder via IMAP `APPEND` — no SMTP,
    /// so it works against any IMAP server. Ensures Drafts exists (`CREATE`, ignoring
    /// "already exists"), appends the assembled RFC 5322 message flagged `\Draft`,
    /// and returns its key (the real Drafts key from UIDPLUS `APPENDUID`, or a
    /// `Message-ID`-derived key the next Drafts sync resolves).
    ///
    /// Unlike a Sent copy this **fails loudly**: saving the draft is the whole operation,
    /// so there is nothing to report success about if the `APPEND` did not land.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] on a transport or `APPEND` failure.
    pub async fn save_draft(&self, draft: &Draft) -> ProviderResult<ProviderKey> {
        // A saved draft retains the Bcc header so resuming it restores every recipient (it is
        // APPENDed locally, never transmitted).
        let message = assemble_filed_message(draft, OffsetDateTime::now_utc())?;
        // The Drafts folder is resolved by its `\Drafts` SPECIAL-USE role (falling back to
        // the conventional "Drafts").
        let (folder, append_uid) = {
            let mut connection = self.connection.lock().await;
            append_to_role_folder(&mut connection, Filing::Drafts, &message).await?
        };
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

#[cfg(test)]
#[path = "filing_tests.rs"]
mod tests;

// The submission-dispatch tests drive a real in-process SMTP server (their own cert +
// TLS harness), so they live in a sibling file to keep `filing_tests.rs` small.
#[cfg(test)]
#[path = "filing_smtp_server_tests.rs"]
mod smtp_server_tests;

// The Sent-copy retry needs a real dial (a mock stream cannot express "this session is dead,
// open another"), so its in-process IMAP + SMTP servers live in their own file.
#[cfg(test)]
#[path = "filing_retry_tests.rs"]
mod retry_tests;
