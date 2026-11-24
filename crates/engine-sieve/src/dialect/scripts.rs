//! Which of an account's Sieve scripts holds its rules: one answer for every protocol that lists
//! them (JMAP's `SieveScript/get`, ManageSieve's `LISTSCRIPTS`).
//!
//! Each adapter says which scripts are a server's own reply script with what its transport can
//! see: JMAP knows the RFC 9661 §4 script by its name (`vacation` on Stalwart), ManageSieve only by
//! its text (an unnamed `vacation`, [`unnamed_vacation`](super::reply::unnamed_vacation)). On
//! Stalwart both point at the same script. Where they would not (a user's script named
//! `vacation` over JMAP; a server's reply script under another name over ManageSieve), the two
//! adapters can choose differently.

/// The name of the script a host creates when the account has no rules script to edit.
pub const RULES_SCRIPT: &str = "rules";

/// One script as the server lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed<'a> {
    /// Its name.
    pub name: &'a str,
    /// Whether it is the active one.
    pub active: bool,
    /// Whether it is a server's own reply script, never the rules'.
    pub server_reply: bool,
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

/// The script that holds the rules: the active one, unless it is a server's reply script; else
/// the script named [`RULES_SCRIPT`], or the one script besides the server's reply scripts.
/// [`Create`](Choice::Create) where there is none, or several and none of them active or named
/// `rules`: which one the user means cannot be told, so a host starts its own rather than pick.
/// A server's reply script is never chosen.
#[must_use]
pub fn rules_script(listed: &[Listed<'_>]) -> Choice {
    if let Some(at) = listed
        .iter()
        .position(|script| script.active && !script.server_reply)
    {
        return Choice::Script(at);
    }
    let others: Vec<usize> = (0..listed.len())
        .filter(|&at| !listed[at].server_reply)
        .collect();
    others
        .iter()
        .copied()
        .find(|&at| listed[at].name == RULES_SCRIPT)
        .or(match others.as_slice() {
            [only] => Some(*only),
            _ => None,
        })
        .map_or(Choice::Create, Choice::Script)
}
