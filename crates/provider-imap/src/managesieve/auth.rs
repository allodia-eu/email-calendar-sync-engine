//! Signing in to a ManageSieve session: `AUTHENTICATE` (RFC 5804 §2.1).
//!
//! ManageSieve has no `LOGIN`, so a password goes as SASL `PLAIN` (RFC 4616) and a token as
//! `OAUTHBEARER` or `XOAUTH2`, chosen from what the server offers once TLS is up, as IMAP chooses
//! (`crate::sasl`). The initial response always rides on the command: RFC 5804 lets every
//! mechanism carry one, so no round trip is spent on an empty challenge. The bytes each mechanism
//! sends are built in `crate::sasl`, once for every protocol here; this module is only the
//! framing, where strings are quoted rather than bare.

use engine_provider::Credential;
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    error::{SieveError, SieveResult},
    response::{Line, ResponseCode, Status, Token},
    session::Session,
};
use crate::{
    credentials::{SignIn, sign_in},
    error::ImapError,
    sasl::{self, Mechanism},
    secret_line::{Part, secret_line},
};

/// How many server challenges one `AUTHENTICATE` may produce before it is abandoned. The
/// mechanisms here take at most one (a token's error challenge); the cap stops a server that
/// answers every acknowledgement with another from holding the sign-in forever.
const MAX_CHALLENGES: usize = 8;

/// A mechanism this client signs in with, and what it answers a challenge with.
#[derive(Clone, Copy)]
enum Chosen {
    Plain,
    Token(Mechanism),
}

impl Chosen {
    const fn atom(self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::Token(mechanism) => mechanism.atom(),
        }
    }

    /// What the client sends when the server answers with a challenge: for a token, the
    /// mechanism's acknowledgement of its error challenge, after which the server reports the
    /// failure; `PLAIN` takes no challenge, so it cancels the exchange (`"*"`, RFC 5804 §2.1).
    const fn answer(self) -> &'static str {
        match self {
            Self::Plain => "*",
            Self::Token(mechanism) => mechanism.cancel_response(),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Session<S> {
    /// Signs in with `credential`, over a mechanism the server offers. `host` and `port` name
    /// the server for an `OAUTHBEARER` response.
    ///
    /// # Errors
    ///
    /// - [`ImapError::AuthUnsupported`] when the server offers no mechanism this credential can
    ///   use, or answers that this user must sign in another way first (`TRANSITION-NEEDED`,
    ///   `AUTH-TOO-WEAK`, `ENCRYPT-NEEDED`): another credential of the kind would meet the same.
    /// - [`ImapError::Auth`] when it refuses the credential, so a replacement may be asked for.
    /// - [`SieveError::Refused`] with `TRYLATER` when it cannot judge the credential now.
    /// - [`ImapError::Bad`] for a credential no mechanism can carry (no username on a token, a NUL
    ///   in a password): nothing is sent.
    pub(crate) async fn authenticate(
        &mut self,
        credential: &Credential,
        host: &str,
        port: Option<u16>,
    ) -> SieveResult<()> {
        let offered = self.capabilities().sasl.clone();
        let offers = |name: &str| offered.iter().any(|atom| atom.eq_ignore_ascii_case(name));
        let (chosen, initial) = match sign_in(credential)? {
            SignIn::Password { username, password } => {
                if !offers("PLAIN") {
                    return Err(unsupported("PLAIN", &offered));
                }
                (
                    Chosen::Plain,
                    sasl::plain_response(None, username, password)?,
                )
            }
            SignIn::Token { username, token } => {
                let mechanism = sasl::select(offered.iter().map(String::as_str))
                    .ok_or_else(|| unsupported("OAuth mechanism", &offered))?;
                let initial = mechanism.initial_response(username, token, host, port)?;
                (Chosen::Token(mechanism), initial)
            }
        };
        let line = secret_line(&[
            Part::Text("AUTHENTICATE \""),
            Part::Text(chosen.atom()),
            Part::Text("\" \""),
            Part::Text(&initial),
            Part::Text("\"\r\n"),
        ]);
        self.send(line.as_bytes()).await?;
        for _ in 0..MAX_CHALLENGES {
            match self.read_line().await? {
                // A challenge: a string on a line of its own. Its text is the server's account
                // of a refused token, and is not kept.
                Line::Data(tokens) if matches!(tokens.as_slice(), [Token::Str(_)]) => {
                    self.send(format!("\"{}\"\r\n", chosen.answer()).as_bytes())
                        .await?;
                }
                Line::Data(_) => {
                    return Err(ImapError::protocol(
                        "an AUTHENTICATE challenge that is not one string",
                    )
                    .into());
                }
                Line::Completion(completion) => {
                    return signed_in(completion.status, completion.code);
                }
            }
        }
        Err(ImapError::protocol(
            "the ManageSieve server kept challenging without completing AUTHENTICATE",
        )
        .into())
    }
}

/// The server offers no mechanism the credential can use: which one was needed, and which were
/// offered (mechanism names are protocol atoms, not content).
fn unsupported(needed: &str, offered: &[String]) -> SieveError {
    let offered = if offered.is_empty() {
        "none".to_owned()
    } else {
        offered.join(" ")
    };
    ImapError::auth_unsupported(format!(
        "the ManageSieve server offers no {needed} (it offers: {offered})"
    ))
    .into()
}

/// An `AUTHENTICATE` completion as the outcome of signing in, by its code alone.
fn signed_in(status: Status, code: Option<ResponseCode>) -> SieveResult<()> {
    match (status, code) {
        (Status::Ok, _) => Ok(()),
        (Status::No, Some(ResponseCode::TryLater)) => Err(SieveError::Refused {
            command: "AUTHENTICATE",
            code,
        }),
        (
            Status::No,
            Some(
                code @ (ResponseCode::TransitionNeeded
                | ResponseCode::AuthTooWeak
                | ResponseCode::EncryptNeeded),
            ),
        ) => Err(ImapError::auth_unsupported(format!(
            "the ManageSieve server will not take this sign-in ({code})"
        ))
        .into()),
        (Status::No, _) => {
            Err(ImapError::auth("the ManageSieve server refused the credential").into())
        }
        (Status::Bye, _) => Err(ImapError::bye(
            "the ManageSieve server closed the connection during AUTHENTICATE",
        )
        .into()),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
