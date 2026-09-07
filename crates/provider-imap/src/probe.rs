//! What a server accepts as proof of identity, read **before** a credential exists.
//!
//! Every other path in this crate starts from a credential a host already holds. This
//! one answers the question that comes first, at account setup: may this account
//! authenticate with a password, with an OAuth 2.0 access token, or with either? Only
//! the server can say, and — like the mechanism choice the SASL layer makes — the answer
//! is a fact about the **server**, never about the provider, so it is read off the wire
//! rather than looked up in a table of provider names.
//!
//! # The dial stops at the answer
//!
//! A probe opens the connection, secures it, reads the pre-authentication `CAPABILITY`
//! (or `EHLO`), and closes. **No credential is sent, and nothing is attempted that the
//! account's provider would record as a failed sign-in.**
//!
//! That rules out one thing worth naming, because it is the obvious next idea: RFC 7628
//! §3.2.2 lets a server describe its OAuth configuration — including an
//! `openid-configuration` URL — in the challenge it returns when it *rejects* a token,
//! so a client could learn where to send the user by presenting a deliberately invalid
//! one. It is not done here. It would leave a failed authentication on the user's
//! account before they had signed in once, on the screen where they are most likely to
//! give up, and the field is optional: Stalwart answers a bad token with a bare
//! `NO [AUTHENTICATIONFAILED]` and no challenge at all. A host discovers the
//! authorization server over HTTPS instead.
//!
//! # Why a probe rather than an inference
//!
//! A host could guess from the autoconfiguration it already fetched. The two disagree
//! often enough to matter: a provider may publish OAuth settings for its web sessions
//! and take only a password on IMAP, or (Microsoft) advertise a password in a database
//! entry years after switching it off. The pre-auth capability line is what the account
//! will actually be judged by, and it costs one connection to the server the account is
//! about to use anyway.

use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream, rustls::pki_types::ServerName};

use crate::{
    config::ImapSecurity,
    dial::open_secured,
    error::{ImapError, ImapResult},
    sasl, smtp,
    smtp_auth::advertised_mechanisms,
};

/// The SASL mechanisms that carry a password. `PLAIN` (RFC 4616) is the one this crate
/// sends; `LOGIN` is the older exchange many servers still list beside it, and its
/// presence says the same thing about the account, which is all a probe reports.
const PASSWORD_MECHANISMS: [&str; 2] = ["PLAIN", "LOGIN"];

/// The capability that withdraws the IMAP `LOGIN` command (RFC 3501 §6.2.3), which a
/// server advertises over a link it considers unfit for a password — or, as Microsoft
/// does, over any link at all.
const LOGIN_DISABLED: &str = "LOGINDISABLED";

/// What a server will accept as proof of identity, from its pre-authentication
/// capability announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthOffer {
    /// Every SASL mechanism advertised, verbatim and in the order the server listed
    /// them. Kept even though the two flags below are what a caller acts on: when both
    /// are `false` this is the only record of what the server *did* want, and that is
    /// the one case somebody has to diagnose.
    pub mechanisms: Vec<String>,
    /// Whether a password is accepted.
    pub password: bool,
    /// Whether an OAuth 2.0 access token is accepted: the server advertised a mechanism
    /// this crate can carry one over: `OAUTHBEARER` (RFC 7628) or `XOAUTH2`.
    pub oauth: bool,
}

/// Reads what the IMAP server at `addr` accepts, presenting `server_name` for TLS.
///
/// On a `StartTls` dial the capability read is the one **after** the upgrade, which is
/// the only one that describes the session an account would actually use: a server
/// commonly withholds its mechanism list, or advertises `LOGINDISABLED`, until the
/// link is secured.
///
/// # Errors
///
/// [`ImapError`](crate::ImapError) on a TCP/TLS failure, a bad server name, or a server
/// that does not answer `CAPABILITY`. A caller treats every one of these the same way:
/// the question went unanswered, so offer what works everywhere (a password) rather
/// than a sign-in that may not exist.
pub async fn probe_imap_auth(
    addr: &str,
    server_name: &str,
    security: ImapSecurity,
    connector: &TlsConnector,
) -> Result<AuthOffer, ImapError> {
    let (mut connection, _tls) = open_secured(addr, server_name, security, connector).await?;
    let capabilities = connection.capabilities().await?;
    // Leave the server a closed session rather than a dropped socket; its reply is
    // irrelevant, and a server that closes without one has still answered.
    let _ = connection.command("LOGOUT").await;
    Ok(imap_offer(&capabilities))
}

/// Reads what the SMTP submission server at `addr` accepts, presenting `server_name`
/// for TLS and identifying as `ehlo_domain`.
///
/// `ehlo_domain` is the account's own mail domain — the same value a real submission
/// sends — because a server may vary what it offers by who is asking, and a probe that
/// introduced itself differently would be answering a different question.
///
/// # Errors
///
/// [`ImapError`](crate::ImapError) on a TCP/TLS failure, a bad server name, a refused
/// `EHLO`, or (on a `StartTls` dial) a server that does not advertise `STARTTLS`. As
/// with the IMAP probe, all of them mean the same thing to a caller.
pub async fn probe_smtp_auth(
    addr: &str,
    server_name: &str,
    security: ImapSecurity,
    ehlo_domain: &str,
    connector: &TlsConnector,
) -> Result<AuthOffer, ImapError> {
    let tcp = TcpStream::connect(addr).await?;
    let extensions = match security {
        ImapSecurity::ImplicitTls => {
            let tls = wrap_tls(connector, server_name, tcp).await?;
            smtp::extensions(tls, ehlo_domain).await?
        }
        ImapSecurity::StartTls => {
            let upgraded = smtp::negotiate_starttls(tcp, ehlo_domain).await?;
            let tls = wrap_tls(connector, server_name, upgraded).await?;
            smtp::extensions_after_starttls(tls, ehlo_domain).await?
        }
    };
    Ok(smtp_offer(&extensions))
}

/// TLS-wraps a submission socket, presenting `server_name`. The IMAP probe gets this
/// from [`open_secured`]; SMTP has no equivalent shared dial, because a submission is
/// one short-lived connection per send rather than a standing session.
async fn wrap_tls(
    connector: &TlsConnector,
    server_name: &str,
    tcp: TcpStream,
) -> ImapResult<TlsStream<TcpStream>> {
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| ImapError::bad(format!("invalid SMTP TLS server name: {e}")))?;
    Ok(connector.connect(name, tcp).await?)
}

/// Classifies an IMAP pre-authentication `CAPABILITY` list.
///
/// A password is offered unless the server both withheld `AUTH=PLAIN`/`AUTH=LOGIN`
/// **and** advertised `LOGINDISABLED`. The two are separate questions: `LOGINDISABLED`
/// withdraws only the `LOGIN` *command*, so a server may disable it and still take a
/// password over SASL, and a server that advertises no `AUTH=` mechanism at all still
/// takes `LOGIN` unless it says otherwise (RFC 3501 §6.2.3).
fn imap_offer(capabilities: &[String]) -> AuthOffer {
    let mechanisms: Vec<String> = capabilities
        .iter()
        .filter_map(|atom| sasl::advertised_mechanism(atom))
        .map(str::to_owned)
        .collect();
    let login_disabled = capabilities
        .iter()
        .any(|atom| atom.eq_ignore_ascii_case(LOGIN_DISABLED));
    AuthOffer {
        password: names_a_password(&mechanisms) || !login_disabled,
        oauth: sasl::select(mechanisms.iter().map(String::as_str)).is_some(),
        mechanisms,
    }
}

/// Classifies an SMTP `EHLO` reply's extension lines.
///
/// Unlike IMAP there is no implicit password command to fall back on: submission
/// authenticates over SASL or not at all (RFC 4954), so an `AUTH` line that names no
/// password mechanism means no password.
fn smtp_offer(extensions: &[String]) -> AuthOffer {
    let mechanisms: Vec<String> = advertised_mechanisms(extensions)
        .into_iter()
        .map(str::to_owned)
        .collect();
    AuthOffer {
        password: names_a_password(&mechanisms),
        oauth: sasl::select(mechanisms.iter().map(String::as_str)).is_some(),
        mechanisms,
    }
}

/// Whether `mechanisms` names one that carries a password. Case-insensitive: mechanism
/// names are protocol atoms.
fn names_a_password(mechanisms: &[String]) -> bool {
    mechanisms.iter().any(|mechanism| {
        PASSWORD_MECHANISMS
            .iter()
            .any(|password| mechanism.eq_ignore_ascii_case(password))
    })
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
