//! SMTP authentication (RFC 4954): `AUTH PLAIN` for a password, `AUTH OAUTHBEARER` /
//! `AUTH XOAUTH2` for an OAuth 2.0 access token.
//!
//! Split from [`crate::smtp`] (which owns the submission *conversation* and is at the
//! file-size limit) but driving the same stream. The mechanism-specific bytes are the
//! ones [`crate::sasl`] builds for IMAP — RFC 4954's initial response and RFC 4959's
//! are the same blob, which is exactly why that module is protocol-neutral.
//!
//! **Authentication only ever runs over an established TLS stream** — implicit TLS, or
//! after a `STARTTLS` upgrade. That invariant is the caller's ([`crate::filing`] never
//! builds an authenticating sender for the plaintext MX transport), and it matters more
//! for a bearer token than for a password: a token grants an account's whole mailbox
//! and is replayable until it expires.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    credentials::Credentials,
    error::{ImapError, ImapResult},
    sasl::{self, Mechanism},
    smtp::SmtpStream,
};

/// What an authenticating submission needs: the credential, plus the server identity a
/// SASL `OAUTHBEARER` response names (RFC 7628 §3.1). `port` is `None` when the
/// configured address carried no parsable one, which drops the pair.
pub(crate) struct SmtpAuth<'a> {
    pub(crate) credentials: &'a Credentials,
    pub(crate) host: &'a str,
    pub(crate) port: Option<u16>,
}

/// Authenticates the session, given the extension lines the server's `EHLO` reply
/// listed.
///
/// # Errors
///
/// [`ImapError::Auth`] if the server rejects the credential, or (for a token) if it
/// advertises no OAuth mechanism — the message names what it did offer, because the
/// usual cause is a token pointed at an account that only takes a password.
pub(crate) async fn authenticate<S>(
    smtp: &mut SmtpStream<S>,
    auth: &SmtpAuth<'_>,
    extensions: &[String],
) -> ImapResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    match auth.credentials {
        Credentials::Password { username, password } => {
            // `AUTH PLAIN` is sent whether or not the server listed it, which is how
            // this path has always behaved; the reply is the answer either way.
            smtp.write_line(&format!("AUTH PLAIN {}", plain_token(username, password)))
                .await?;
            let (code, text) = smtp.read_reply().await?;
            if code != 235 {
                return Err(ImapError::auth(format!(
                    "SMTP AUTH rejected: {code} {text}"
                )));
            }
            Ok(())
        }
        Credentials::OAuth2 {
            username,
            access_token,
        } => {
            let offered = advertised_mechanisms(extensions);
            let mechanism = sasl::select(offered.iter().copied()).ok_or_else(|| {
                ImapError::auth(format!(
                    "SMTP server advertises no OAuth SASL mechanism (it offers: {})",
                    if offered.is_empty() {
                        "none".to_owned()
                    } else {
                        offered.join(" ")
                    }
                ))
            })?;
            let initial =
                mechanism.initial_response(username, access_token, auth.host, auth.port)?;
            smtp.write_line(&format!("AUTH {} {initial}", mechanism.atom()))
                .await?;
            finish(smtp, mechanism).await
        }
    }
}

/// Reads the answer to an OAuth `AUTH`, acknowledging an error challenge so the server
/// will report the failure.
///
/// The `334` is not a request for more credential — both mechanisms send the whole
/// thing up front — it is the rejection, described in base64 JSON. The server then
/// waits for an acknowledgement before issuing the real `535`, so a client that skips
/// it turns a stale token into a stalled connection rather than an error a host can
/// act on (RFC 7628 §3.2.3).
async fn finish<S>(smtp: &mut SmtpStream<S>, mechanism: Mechanism) -> ImapResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (code, text) = smtp.read_reply().await?;
    if code == 235 {
        return Ok(());
    }
    if code != 334 {
        return Err(ImapError::auth(format!(
            "SMTP AUTH rejected: {code} {text}"
        )));
    }
    let challenge = sasl::describe_challenge(&text);
    smtp.write_line(mechanism.cancel_response()).await?;
    let (code, text) = smtp.read_reply().await?;
    let described = if challenge.is_empty() {
        text
    } else {
        format!("{text} ({challenge})")
    };
    Err(ImapError::auth(format!(
        "SMTP AUTH rejected: {code} {described}"
    )))
}

/// The SASL mechanisms an `EHLO` reply advertised, from its `AUTH …` line (RFC 4954
/// §2.2 — also accepting the legacy `AUTH=…` spelling some servers still emit for old
/// clients).
///
/// Read **per line**, never from the joined reply text: the extension keyword is only
/// meaningful at the start of its own line, so joining first lets a mechanism name in
/// one line's prose be read as another's.
fn advertised_mechanisms(extensions: &[String]) -> Vec<&str> {
    extensions
        .iter()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let keyword = words.next()?;
            let rest: Vec<&str> = words.collect();
            if keyword.eq_ignore_ascii_case("AUTH") {
                return Some(rest);
            }
            // `AUTH=PLAIN LOGIN`: the first mechanism is glued to the keyword.
            let (prefix, first) = keyword.split_at_checked("AUTH=".len())?;
            prefix.eq_ignore_ascii_case("AUTH=").then(|| {
                let mut all = vec![first];
                all.extend(rest);
                all
            })
        })
        .flatten()
        .collect()
}

/// The `AUTH PLAIN` SASL token: base64 of `\0user\0password` (RFC 4616).
fn plain_token(username: &str, password: &str) -> String {
    let mut credentials = vec![0u8];
    credentials.extend_from_slice(username.as_bytes());
    credentials.push(0);
    credentials.extend_from_slice(password.as_bytes());
    crate::base64::encode(&credentials)
}

#[cfg(test)]
#[path = "smtp_auth_tests.rs"]
mod tests;
