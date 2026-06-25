//! SMTP submission + IMAP `APPEND` filing of sent copies and drafts.
//!
//! The submission *conversation* lives in [`crate::smtp`]; this module is the
//! `Provider`-side glue that runs it and files the resulting copy into the account's
//! real Sent/Drafts folder (resolved by SPECIAL-USE role, `imap-smtp.md`). It is the
//! [`ImapProvider`] half that `submit_email` delegates to, kept out of
//! [`crate::provider`] so that file stays under the size limit.

use engine_core::ids::{MessageIdHeader, ProviderKey};
use engine_core::mail::MailboxRole;
use engine_provider::{Draft, ProviderError, ProviderResult, SubmissionReceipt};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::ImapResult;
use crate::mail::{mailbox_from_list, message_key};
use crate::provider::ImapProvider;
use crate::smtp::{self, Disposition};
use crate::transport::Connection;

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
    /// The submission core over an arbitrary SMTP stream — the seam the offline
    /// tests drive with a mock while [`Provider::submit_email`](engine_provider::Provider::submit_email)
    /// supplies a TCP (or TLS) socket. Runs the conversation (optionally
    /// authenticating with `auth`), maps the disposition to a result/classified
    /// error, then files the Sent copy via the IMAP connection.
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
