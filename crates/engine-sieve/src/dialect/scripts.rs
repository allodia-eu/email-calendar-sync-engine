//! Which of an account's Sieve scripts holds its rules: one answer for every protocol that lists
//! them (JMAP's `SieveScript/get`, ManageSieve's `LISTSCRIPTS`).
//!
//! Each adapter says which scripts are a server's own reply script with what its transport can
//! see: JMAP knows the RFC 9661 §4 script by its name (`vacation` on Stalwart), ManageSieve only by
//! its text (an unnamed `vacation`, [`unnamed_vacation`](super::reply::unnamed_vacation)). On
//! Stalwart both point at the same script. Where they would not (a user's script named
//! `vacation` over JMAP; a server's reply script under another name over ManageSieve), the two
//! adapters can choose differently.

use crate::{TokenKind, segment::SegmentKind};

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
/// does not say and the order needs it: of the active script, of `rules`, of the one candidate
/// left. A script never asked counts as a candidate, so two scripts never read as one. An error
/// from `server_reply` ends the choice.
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
    let others: Vec<usize> = (0..listed.len())
        .filter(|&at| candidate(&known, at))
        .collect();
    if let [only] = others.as_slice()
        && !is_server(&mut known, *only)?
    {
        return Ok(Choice::Script(*only));
    }
    Ok(Choice::Create)
}

/// Whether `script` is plainly a server's own reply script: nothing but `require`s, comments and
/// one top-level command no `# rule:[…]` name marks, made only of `if` tests on `currentdate` (or
/// `true`) around one `vacation`. That is the shape Stalwart writes for its JMAP reply, captured in
/// both its forms (one bound, two nested). Any other script, one with a filter beside its reply
/// (as SOGo writes them), one written in Roundcube's dialect, or one that cannot be split, is no
/// server's: a host does not route rules around it.
#[must_use]
pub fn plainly_server_reply(script: &str) -> bool {
    let Ok(segments) = crate::segment(script) else {
        return false;
    };
    let commands: Vec<_> = segments
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Command)
        .collect();
    let [command] = commands.as_slice() else {
        return false;
    };
    let identifiers: Vec<_> = command
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Identifier)
        .collect();
    super::read::name(command.leading()).is_none()
        && identifiers
            .iter()
            .filter(|token| token.is_identifier("vacation"))
            .count()
            == 1
        && identifiers.iter().all(|token| {
            ["if", "currentdate", "allof", "true", "vacation"]
                .iter()
                .any(|name| token.is_identifier(name))
        })
}
