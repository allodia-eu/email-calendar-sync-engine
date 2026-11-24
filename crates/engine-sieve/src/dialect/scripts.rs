//! Which of an account's Sieve scripts holds its rules: one answer for every protocol that lists
//! them (JMAP's `SieveScript/get`, ManageSieve's `LISTSCRIPTS`).
//!
//! Each adapter says which scripts are a server's own reply script with what its transport can
//! see: JMAP knows the RFC 9661 §4 script by its name (`vacation` on Stalwart), ManageSieve only by
//! its text, in the exact shape Stalwart writes ([`plainly_server_reply`]). On
//! Stalwart both point at the same script. Where they would not (a user's script named
//! `vacation` over JMAP; a server's reply script under another name over ManageSieve), the two
//! adapters can choose differently.

use crate::{Token, TokenKind, segment::SegmentKind};

/// The name of the script a host creates when the account has no rules script to edit.
pub const RULES_SCRIPT: &str = "rules";

/// One script as the server lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed<'a> {
    /// Its name.
    pub name: &'a str,
    /// Whether it is the active one.
    pub active: bool,
    /// Whether it is a server's own reply script, never the rules', where the adapter knows
    /// without asking (JMAP, by its name); `None` where it must read the script to tell
    /// ([`choose_rules_script`] asks).
    pub server_reply: Option<bool>,
}

/// Where the rules are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Choice {
    /// In this script, by its index among the scripts listed.
    Script(usize),
    /// In none: a write creates [`RULES_SCRIPT`].
    Create,
}

/// [`choose_rules_script`] where every script's kind is known.
#[must_use]
pub fn rules_script(listed: &[Listed<'_>]) -> Choice {
    match choose_rules_script(listed, |_| Ok::<_, core::convert::Infallible>(false)) {
        Ok(choice) => choice,
        Err(never) => match never {},
    }
}

/// The script that holds the rules: the active one, unless it is a server's reply script; else
/// the script named [`RULES_SCRIPT`]; else the one script besides the server's reply scripts.
/// [`Create`](Choice::Create) where there is none, or several and none of them active or named
/// `rules`: which one the user means cannot be told, so a host starts its own rather than pick.
/// A server's reply script is never chosen.
///
/// Whether a script is a server's reply script is asked of `server_reply` only where the listing
/// does not say and the order needs it: of the active script, of `rules`, and of the candidates
/// left (each of them, where several are), so a protocol that must read a script to tell
/// chooses as one that knows by name does. An error from `server_reply` ends the choice.
///
/// # Errors
///
/// What `server_reply` answers.
pub fn choose_rules_script<E>(
    listed: &[Listed<'_>],
    mut server_reply: impl FnMut(usize) -> Result<bool, E>,
) -> Result<Choice, E> {
    let mut known: Vec<Option<bool>> = listed.iter().map(|script| script.server_reply).collect();
    let mut is_server = |known: &mut Vec<Option<bool>>, at: usize| -> Result<bool, E> {
        if let Some(server) = known[at] {
            return Ok(server);
        }
        let server = server_reply(at)?;
        known[at] = Some(server);
        Ok(server)
    };
    if let Some(at) = listed.iter().position(|script| script.active)
        && !is_server(&mut known, at)?
    {
        return Ok(Choice::Script(at));
    }
    let candidate = |known: &[Option<bool>], at: usize| known[at] != Some(true);
    if let Some(at) =
        (0..listed.len()).find(|&at| candidate(&known, at) && listed[at].name == RULES_SCRIPT)
        && !is_server(&mut known, at)?
    {
        return Ok(Choice::Script(at));
    }
    let mut others: Vec<usize> = (0..listed.len())
        .filter(|&at| candidate(&known, at))
        .collect();
    if others.len() > 1 {
        // Several left: one of them may be a server's reply script, and the rest only one script.
        for &at in &others {
            is_server(&mut known, at)?;
        }
        others.retain(|&at| candidate(&known, at));
    }
    if let [only] = others.as_slice()
        && !is_server(&mut known, *only)?
    {
        return Ok(Choice::Script(*only));
    }
    Ok(Choice::Create)
}

/// The extensions a server's own reply script requires, in every window shape: Stalwart requires
/// all three even with no window.
const SERVER_REPLY_REQUIRES: [&str; 3] = ["vacation", "relational", "date"];

/// Whether `script` is plainly a server's own reply script, in exactly the shape Stalwart writes
/// for its JMAP `VacationResponse` (captured live in every window shape: none, a start, an end,
/// both; `tests/fixtures/stalwart/`):
///
/// - one `require` of exactly `vacation`, `relational` and `date`;
/// - one top-level command no `# rule:[…]` name marks: up to two nested `if currentdate :value
///   "ge"|"le" "iso8601" "…" {`, the start outside the end, with no `else`, around one `vacation`
///   that carries `:mime`;
/// - nothing else.
///
/// Any other script is someone's rules: a user's own reply script (`vacation :days 7 "Away";`),
/// one with filters beside its reply (as SOGo writes them), one in Roundcube's dialect, or one
/// that cannot be split. A host does not route rules around it.
#[must_use]
pub fn plainly_server_reply(script: &str) -> bool {
    let Ok(segments) = crate::segment(script) else {
        return false;
    };
    let mut requires = segments
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Require);
    let mut commands = segments
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Command);
    let (Some(require), None, Some(command), None) = (
        requires.next(),
        requires.next(),
        commands.next(),
        commands.next(),
    ) else {
        return false;
    };
    let Ok(mut names) = crate::require::capabilities(require) else {
        return false;
    };
    names.sort_unstable();
    let mut expected = SERVER_REPLY_REQUIRES.map(str::to_owned);
    expected.sort_unstable();
    super::read::name(command.leading()).is_none()
        && names == expected
        && windowed_mime_vacation(&spoken(command.tokens()))
}

/// `tokens` without white space; a comment stays, so that it fails Stalwart's shape.
fn spoken<'a>(tokens: &[Token<'a>]) -> Vec<Token<'a>> {
    tokens
        .iter()
        .filter(|token| token.kind() != TokenKind::Whitespace)
        .copied()
        .collect()
}

/// Whether `tokens` are Stalwart's window around its reply: up to two `currentdate` bounds, the
/// start (`ge`) outside the end (`le`), then a `vacation` with `:mime`, then the closing braces.
fn windowed_mime_vacation(tokens: &[Token<'_>]) -> bool {
    if tokens.iter().any(|token| token.kind().is_trivia()) {
        return false;
    }
    let mut rest = tokens;
    let mut bounds = Vec::new();
    while let [if_, test, value, comparator, format, date, open, after @ ..] = rest
        && if_.is_identifier("if")
    {
        let comparator = crate::string::value(comparator);
        let bound = matches!(comparator.as_deref(), Some("ge" | "le"));
        if !(test.is_identifier("currentdate")
            && value.is_tag("value")
            && bound
            && crate::string::value(format).as_deref() == Some("iso8601")
            && date.kind() == TokenKind::QuotedString
            && open.kind() == TokenKind::LeftBrace)
        {
            return false;
        }
        bounds.push(comparator);
        rest = after;
    }
    let ordered = matches!(
        bounds
            .iter()
            .map(Option::as_deref)
            .collect::<Vec<_>>()
            .as_slice(),
        [] | [Some("ge" | "le")] | [Some("ge"), Some("le")]
    );
    let Some(end) = rest
        .iter()
        .position(|token| token.kind() == TokenKind::Semicolon)
    else {
        return false;
    };
    let (vacation, closing) = rest.split_at(end);
    // What follows the reply: the braces its bounds opened, and nothing else (segmenting the
    // script has already matched them).
    let closing = &closing[1..];
    ordered
        && vacation
            .first()
            .is_some_and(|token| token.is_identifier("vacation"))
        && vacation.iter().any(|token| token.is_tag("mime"))
        && vacation[1..]
            .iter()
            .all(|token| token.kind() != TokenKind::Identifier)
        && closing
            .iter()
            .all(|token| token.kind() == TokenKind::RightBrace)
}
