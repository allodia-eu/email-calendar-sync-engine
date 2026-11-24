//! What a ManageSieve server answers (RFC 5804 §4), read off one logical line at a time.
//!
//! A response is any number of data lines, each a list of strings and atoms, and then a
//! completion: `OK`, `NO` or `BYE`, an optional response code in parentheses, and an optional
//! human-readable string. The data lines carry the capabilities, the script list and a script's
//! text. The completion's string is the server's prose: it can quote the script it refused, so it
//! is skipped here and never reaches an error. Only the response code is kept.
//!
//! Lines arrive with any literal already inlined (`crate::transport_read::read_framed_line`), so
//! the lexer here sees `{n}` or `{n+}`, a CRLF, and the n bytes, in place.

use core::fmt;

use crate::error::{ImapError, ImapResult};

/// One item of a response line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// A bare atom: `OK`, `NO`, `BYE`, `ACTIVE`, a response code.
    Atom(String),
    /// A quoted string or a literal, its value unescaped.
    Str(String),
    /// `(`, opening a response code.
    Open,
    /// `)`, closing it.
    Close,
}

/// How a command completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// The command succeeded.
    Ok,
    /// The command failed; the session goes on.
    No,
    /// The server is closing the connection.
    Bye,
}

/// A completion's response code (RFC 5804 §1.3). The ones a client acts on are named; any other
/// is [`Other`](Self::Other), without its name, since nothing here reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseCode {
    /// `AUTH-TOO-WEAK`: the mechanism is too weak for this user.
    AuthTooWeak,
    /// `ENCRYPT-NEEDED`: the mechanism needs a security layer the client did not set up.
    EncryptNeeded,
    /// `QUOTA`, `QUOTA/MAXSCRIPTS` or `QUOTA/MAXSIZE`.
    Quota(Quota),
    /// `TRANSITION-NEEDED`: the user must sign in another way once before this mechanism works.
    TransitionNeeded,
    /// `TRYLATER`: a temporary failure; the same command may succeed later.
    TryLater,
    /// `ACTIVE`: the command cannot act on the active script.
    Active,
    /// `NONEXISTENT`: the script named does not exist.
    Nonexistent,
    /// `ALREADYEXISTS`: a script of that name exists already.
    AlreadyExists,
    /// `WARNINGS`: the script was stored, and the server has something to say about it.
    Warnings,
    /// Any other code (`REFERRAL`, `SASL`, `TAG`, or an extension's).
    Other,
}

/// Which quota a `QUOTA` code says was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quota {
    /// `QUOTA`: the server does not say which.
    Unspecified,
    /// `QUOTA/MAXSCRIPTS`: the account holds as many scripts as it may.
    MaxScripts,
    /// `QUOTA/MAXSIZE`: the script, or all scripts together, would be too large.
    MaxSize,
}

impl fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AuthTooWeak => "AUTH-TOO-WEAK",
            Self::EncryptNeeded => "ENCRYPT-NEEDED",
            Self::Quota(Quota::Unspecified) => "QUOTA",
            Self::Quota(Quota::MaxScripts) => "QUOTA/MAXSCRIPTS",
            Self::Quota(Quota::MaxSize) => "QUOTA/MAXSIZE",
            Self::TransitionNeeded => "TRANSITION-NEEDED",
            Self::TryLater => "TRYLATER",
            Self::Active => "ACTIVE",
            Self::Nonexistent => "NONEXISTENT",
            Self::AlreadyExists => "ALREADYEXISTS",
            Self::Warnings => "WARNINGS",
            Self::Other => "an extension code",
        })
    }
}

impl ResponseCode {
    fn from_atom(atom: &str) -> Self {
        match atom.to_ascii_uppercase().as_str() {
            "AUTH-TOO-WEAK" => Self::AuthTooWeak,
            "ENCRYPT-NEEDED" => Self::EncryptNeeded,
            "QUOTA" => Self::Quota(Quota::Unspecified),
            "QUOTA/MAXSCRIPTS" => Self::Quota(Quota::MaxScripts),
            "QUOTA/MAXSIZE" => Self::Quota(Quota::MaxSize),
            "TRANSITION-NEEDED" => Self::TransitionNeeded,
            "TRYLATER" => Self::TryLater,
            "ACTIVE" => Self::Active,
            "NONEXISTENT" => Self::Nonexistent,
            "ALREADYEXISTS" => Self::AlreadyExists,
            "WARNINGS" => Self::Warnings,
            _ => Self::Other,
        }
    }
}

/// A command's completion: its status and code. The server's prose is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completion {
    pub(crate) status: Status,
    pub(crate) code: Option<ResponseCode>,
}

/// One logical line of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    /// A data line: its strings and atoms.
    Data(Vec<Token>),
    /// The completion that ends the response.
    Completion(Completion),
}

/// Reads one logical line (its literals inlined, its CRLF included).
///
/// # Errors
///
/// [`ImapError::Protocol`] for a line that is not ManageSieve: an unterminated string, a literal
/// whose length is not what follows it, a string that is not UTF-8. The error names the fault,
/// never the line.
pub(crate) fn parse_line(line: &[u8]) -> ImapResult<Line> {
    let raw = tokenize(line)?;
    let atom = |at: usize| match raw.get(at) {
        Some(Raw::Atom(bytes)) => core::str::from_utf8(bytes).ok(),
        _ => None,
    };
    let status = match atom(0) {
        Some(word) if word.eq_ignore_ascii_case("OK") => Status::Ok,
        Some(word) if word.eq_ignore_ascii_case("NO") => Status::No,
        Some(word) if word.eq_ignore_ascii_case("BYE") => Status::Bye,
        _ => {
            let tokens = raw
                .into_iter()
                .map(Raw::into_token)
                .collect::<ImapResult<_>>()?;
            return Ok(Line::Data(tokens));
        }
    };
    // The prose after the code is the server's own and may be in any charset: it is neither
    // kept nor required to be text.
    let code = match (raw.get(1), atom(2)) {
        (Some(Raw::Open), Some(code)) => Some(ResponseCode::from_atom(code)),
        (Some(Raw::Open), None) => return Err(malformed("a response code that is not an atom")),
        _ => None,
    };
    Ok(Line::Completion(Completion { status, code }))
}

/// A token as lexed, before its bytes are read as text.
enum Raw {
    Atom(Vec<u8>),
    Str(Vec<u8>),
    Open,
    Close,
}

impl Raw {
    fn into_token(self) -> ImapResult<Token> {
        Ok(match self {
            Self::Atom(bytes) => Token::Atom(text(bytes)?),
            Self::Str(bytes) => Token::Str(text(bytes)?),
            Self::Open => Token::Open,
            Self::Close => Token::Close,
        })
    }
}

fn malformed(what: &str) -> ImapError {
    ImapError::protocol(format!("a ManageSieve response with {what}"))
}

/// The tokens of one line. A completion's text after its code is lexed too, but never kept.
fn tokenize(line: &[u8]) -> ImapResult<Vec<Raw>> {
    let body = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .ok_or_else(|| malformed("no line end"))?;
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < body.len() {
        match body[at] {
            b' ' => at += 1,
            b'(' => {
                tokens.push(Raw::Open);
                at += 1;
            }
            b')' => {
                tokens.push(Raw::Close);
                at += 1;
            }
            b'"' => {
                let (value, next) = quoted(body, at + 1)?;
                tokens.push(Raw::Str(value));
                at = next;
            }
            b'{' => {
                let (value, next) = literal(body, at)?;
                tokens.push(Raw::Str(value.to_vec()));
                at = next;
            }
            _ => {
                let end = body[at..]
                    .iter()
                    .position(|&byte| matches!(byte, b' ' | b'(' | b')' | b'"' | b'{'))
                    .map_or(body.len(), |offset| at + offset);
                tokens.push(Raw::Atom(body[at..end].to_vec()));
                at = end;
            }
        }
    }
    Ok(tokens)
}

/// A quoted string's value, from just after its opening quote, and where the line goes on.
fn quoted(body: &[u8], mut at: usize) -> ImapResult<(Vec<u8>, usize)> {
    let mut value = Vec::new();
    while let Some(&byte) = body.get(at) {
        match byte {
            b'"' => return Ok((value, at + 1)),
            b'\\' => {
                let escaped = *body
                    .get(at + 1)
                    .ok_or_else(|| malformed("an unterminated string"))?;
                value.push(escaped);
                at += 2;
            }
            b'\r' | b'\n' => break,
            _ => {
                value.push(byte);
                at += 1;
            }
        }
    }
    Err(malformed("an unterminated string"))
}

/// A literal's bytes, from its opening brace, and where the line goes on.
fn literal(body: &[u8], at: usize) -> ImapResult<(&[u8], usize)> {
    let close = body[at..]
        .iter()
        .position(|&byte| byte == b'}')
        .map(|offset| at + offset)
        .ok_or_else(|| malformed("an unterminated literal"))?;
    let digits = body[at + 1..close]
        .strip_suffix(b"+")
        .unwrap_or(&body[at + 1..close]);
    let len: usize = core::str::from_utf8(digits)
        .ok()
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| malformed("a literal whose length is not a number"))?;
    let start = close + 1;
    let start = body
        .get(start..start + 2)
        .filter(|crlf| *crlf == b"\r\n")
        .map(|_| start + 2)
        .ok_or_else(|| malformed("a literal not followed by a line end"))?;
    let end = start
        .checked_add(len)
        .filter(|&end| end <= body.len())
        .ok_or_else(|| malformed("a literal shorter than it announced"))?;
    Ok((&body[start..end], end))
}

fn text(bytes: Vec<u8>) -> ImapResult<String> {
    String::from_utf8(bytes).map_err(|_| malformed("a string that is not UTF-8"))
}

/// What a server says it offers (RFC 5804 §1.7): sent unasked on connect and after `STARTTLS`,
/// and in answer to `CAPABILITY`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// The SASL mechanisms on offer, as named; empty before TLS on both harness servers.
    pub(crate) sasl: Vec<String>,
    /// The Sieve extensions the server's interpreter supports, as named.
    pub(crate) sieve: Vec<String>,
    /// Whether `STARTTLS` is offered.
    pub(crate) starttls: bool,
    /// The most redirects one message may cause, where the server says (Dovecot says so only
    /// once the user has signed in).
    pub(crate) max_redirects: Option<usize>,
}

impl Capabilities {
    /// The capabilities in a response's data lines. A line with a name this client does not use
    /// is skipped; one that is not a string, or a value that is not one, is refused.
    pub(crate) fn from_lines(lines: &[Vec<Token>]) -> ImapResult<Self> {
        let mut capabilities = Self::default();
        for line in lines {
            let (name, value) = match line.as_slice() {
                [Token::Str(name)] => (name, None),
                [Token::Str(name), Token::Str(value)] => (name, Some(value.as_str())),
                _ => return Err(malformed("a capability that is not a string")),
            };
            let words = || {
                value
                    .unwrap_or_default()
                    .split_ascii_whitespace()
                    .map(str::to_owned)
                    .collect()
            };
            match name.to_ascii_uppercase().as_str() {
                "SASL" => capabilities.sasl = words(),
                "SIEVE" => capabilities.sieve = words(),
                "STARTTLS" => capabilities.starttls = true,
                "MAXREDIRECTS" => {
                    capabilities.max_redirects = Some(
                        value
                            .and_then(|count| count.parse().ok())
                            .ok_or_else(|| malformed("a MAXREDIRECTS that is not a number"))?,
                    );
                }
                _ => {}
            }
        }
        Ok(capabilities)
    }

    /// Whether the server lists the Sieve extension `name` (case-insensitive, as RFC 5228 §2.7.2
    /// compares capability strings).
    pub(crate) fn lists(&self, name: &str) -> bool {
        self.sieve
            .iter()
            .any(|extension| extension.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;
