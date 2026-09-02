//! The two SASL mechanisms that carry an OAuth 2.0 access token: `OAUTHBEARER`
//! (RFC 7628) and `XOAUTH2` (Google's earlier, still-dominant vendor mechanism).
//!
//! Pure string and base64 work, no I/O. Both protocols this crate speaks carry the
//! *same* bytes — IMAP as `AUTHENTICATE <mech> <base64>` (RFC 4959) and SMTP as
//! `AUTH <mech> <base64>` (RFC 4954) — so the initial client response is built once
//! here and the framing belongs to the caller ([`crate::transport_auth`] and
//! [`crate::smtp_auth`]).
//!
//! **Which mechanism to use is a fact about the server, never about the provider.** A
//! host hands the engine a username and an access token and nothing else; the adapter
//! reads the offered mechanisms off `CAPABILITY`/`EHLO` and picks one. Nothing above
//! this crate has to know that Microsoft's IMAP offers only `XOAUTH2` while Gmail and
//! Yahoo offer both — a host that had to ask "which provider is this?" would be the
//! engine leaking its job outward
//! (`docs/agent-guidance/imap-smtp.md`).

use crate::error::{ImapError, ImapResult};

/// A SASL mechanism that presents an OAuth 2.0 bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mechanism {
    /// `OAUTHBEARER` (RFC 7628) — the IETF-standard mechanism.
    OAuthBearer,
    /// `XOAUTH2` — Google's mechanism, predating RFC 7628 and adopted by Microsoft and
    /// Yahoo. Not an RFC; the reference is Google's published protocol page.
    XOAuth2,
}

/// The mechanisms this client can present, **in preference order**.
///
/// `OAUTHBEARER` leads, and the reason is testability rather than taste. The two carry
/// the identical token, so on a server offering both the choice decides only which of
/// our two code paths ever runs — and every server we can actually reach offers both.
/// Their **observed** pre-auth capability lines (captured in `sasl_tests.rs`) say so:
/// Gmail advertises `AUTH=XOAUTH2 … AUTH=OAUTHBEARER`, and so does Yahoo, whose own
/// documentation describes only `OAUTHBEARER`. Preferring `XOAUTH2` would therefore
/// leave `OAUTHBEARER` running against **no** live server at all.
///
/// That matters because `OAUTHBEARER` is the path more likely to be subtly wrong: it is
/// the newer one (RFC 7628, 2015), it carries a GS2 header and `host`/`port` pairs
/// `XOAUTH2` does not, and its rejection is acknowledged with `AQ==` rather than an
/// empty line. `XOAUTH2` is a decade-old de-facto standard whose exact bytes Google
/// publishes and this crate pins offline. Given one live proof to spend, it buys more
/// on the standard mechanism — so the preferred path is the tested path, and `XOAUTH2`
/// remains the fallback for a server that offers only it (Microsoft 365, per its
/// documentation).
///
/// The order is deliberately *not* a host-visible knob: a caller that could pin a
/// mechanism would be encoding which provider it is talking to.
const SUPPORTED: [Mechanism; 2] = [Mechanism::OAuthBearer, Mechanism::XOAuth2];

/// The longest server challenge text carried into an error message. A rejected token
/// is answered with a short JSON object; anything beyond this is a server misbehaving,
/// and an unbounded error string is an allocation an adversary controls.
const MAX_CHALLENGE_DETAIL: usize = 512;

impl Mechanism {
    /// The mechanism name as it appears in `CAPABILITY` (after `AUTH=`), in an
    /// `EHLO` `AUTH` list, and on the command line itself.
    pub(crate) const fn atom(self) -> &'static str {
        match self {
            Self::OAuthBearer => "OAUTHBEARER",
            Self::XOAuth2 => "XOAUTH2",
        }
    }

    /// The base64 **initial client response** — the credential blob both protocols
    /// send.
    ///
    /// `OAUTHBEARER` (RFC 7628 §3.1) is a GS2 header naming the authorization identity
    /// followed by `%x01`-separated key/value pairs, of which only `auth` is required;
    /// `host` and `port` are sent because that is the shape every deployed server
    /// documents (Yahoo's example included), and `port` is omitted when the dial
    /// address carried no parsable one. `XOAUTH2` is the flatter
    /// `user=…^Aauth=Bearer …^A^A`.
    ///
    /// # Errors
    ///
    /// [`ImapError::Protocol`] if any component carries a byte that would forge the
    /// frame — see [`clean`].
    pub(crate) fn initial_response(
        self,
        username: &str,
        access_token: &str,
        host: &str,
        port: Option<u16>,
    ) -> ImapResult<String> {
        let username = clean("username", username)?;
        let access_token = clean("access token", access_token)?;
        let host = clean("host", host)?;
        let blob = match self {
            Self::OAuthBearer => {
                // `port` is a `u16`, so its rendering needs no screening.
                let port = port
                    .map(|port| format!("port={port}\x01"))
                    .unwrap_or_default();
                format!(
                    "n,a={username},\x01host={host}\x01{port}\
                     auth=Bearer {access_token}\x01\x01"
                )
            }
            Self::XOAuth2 => format!("user={username}\x01auth=Bearer {access_token}\x01\x01"),
        };
        Ok(crate::base64::encode(blob.as_bytes()))
    }

    /// What the client sends when the server answers the credential with an **error
    /// challenge** instead of a completion.
    ///
    /// Both mechanisms describe the rejection in a base64 JSON challenge and then wait
    /// for the client to acknowledge before reporting the failure through the protocol's
    /// own error path. The acknowledgement differs: RFC 7628 §3.2.3 specifies a single
    /// `%x01` (`AQ==`), while `XOAUTH2` takes an empty line. Sending neither leaves the
    /// connection parked mid-SASL, which is how a wrong token turns into a hang rather
    /// than an authentication error.
    pub(crate) const fn cancel_response(self) -> &'static str {
        match self {
            Self::OAuthBearer => "AQ==",
            Self::XOAuth2 => "",
        }
    }
}

/// Picks the mechanism to present from the ones a server `offered`, or `None` when it
/// offered neither (an account configured with a token against a server that takes
/// only passwords). Comparison is ASCII-case-insensitive: mechanism names are
/// protocol atoms.
pub(crate) fn select<'a>(offered: impl IntoIterator<Item = &'a str>) -> Option<Mechanism> {
    let offered: Vec<&str> = offered.into_iter().collect();
    SUPPORTED.into_iter().find(|mechanism| {
        offered
            .iter()
            .any(|name| name.eq_ignore_ascii_case(mechanism.atom()))
    })
}

/// Renders a server error challenge as failure detail.
///
/// The challenge is base64 JSON (`{"status":"invalid_token",…}`) and is the only place
/// the server says *why* a token was refused — expired, wrong scope, wrong account —
/// so it is worth carrying into the error a host sees. It is hostile input: it is
/// decoded leniently (an undecodable challenge is reported verbatim), stripped of
/// control characters so it cannot forge log lines, and truncated.
pub(crate) fn describe_challenge(challenge: &str) -> String {
    let trimmed = challenge.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let decoded = crate::base64::decode(trimmed).map_or_else(
        || trimmed.to_owned(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    );
    decoded
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_CHALLENGE_DETAIL)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Rejects a credential component carrying a byte that would forge the SASL frame.
///
/// `%x01` is the key/value separator itself, so a token containing one could append
/// its own `auth=` pair; NUL, CR and LF would break out of the command line the base64
/// blob rides on (the same class of guard `crate::smtp` applies to envelope addresses
/// and `engine-rfc5322` to header values). Screening happens *before* base64 encoding,
/// which is what makes it a guard rather than a formality.
fn clean<'a>(field: &str, value: &'a str) -> ImapResult<&'a str> {
    if value
        .bytes()
        .any(|byte| matches!(byte, 0x00 | 0x01 | b'\r' | b'\n'))
    {
        return Err(ImapError::protocol(format!(
            "{field} contains a byte forbidden in a SASL response (NUL, SOH, CR, or LF)"
        )));
    }
    Ok(value)
}

#[cfg(test)]
#[path = "sasl_tests.rs"]
mod tests;
