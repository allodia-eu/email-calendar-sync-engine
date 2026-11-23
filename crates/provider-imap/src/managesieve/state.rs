//! An account's Sieve scripts as a rule editor sees them: which script holds the rules (and, on
//! ManageSieve, the out-of-office reply), its text, and the version a write is guarded by.
//!
//! ManageSieve keeps no version of a script and has no conditional `PUTSCRIPT`, so the version is
//! the engine's: a SHA-256 of the script list (each name and whether it is active) and the text
//! of the script that holds the rules. A write lists and reads them again in its own session and
//! refuses when the hash moved, which narrows the race with another client to the one round trip
//! between that read and `PUTSCRIPT`; it does not close it (`WriteGuard::Absent`).

use core::fmt::Write as _;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    error::SieveResult,
    name::ScriptName,
    session::{ListedScript, Session},
};

/// The name of the script the engine creates when the account has none to edit, as on JMAP
/// (`filters-and-out-of-office.md`, decision 3).
pub(crate) const RULES_SCRIPT: &str = "rules";

/// The account's scripts as listed, the one that holds the rules, and its text.
pub(crate) struct State {
    pub(crate) listed: Vec<ListedScript>,
    /// The script holding the rules, or `None` where a write creates [`RULES_SCRIPT`].
    pub(crate) script: Option<ListedScript>,
    /// Its text; empty where there is none.
    pub(crate) text: String,
}

impl State {
    /// Lists the scripts and reads the one that holds the rules.
    pub(crate) async fn read<S: AsyncRead + AsyncWrite + Unpin + Send>(
        session: &mut Session<S>,
    ) -> SieveResult<Self> {
        let listed = session.list_scripts().await?;
        let script = holding_rules(&listed).cloned();
        let text = match &script {
            Some(script) => session.get_script(&script.name).await?,
            None => String::new(),
        };
        Ok(Self {
            listed,
            script,
            text,
        })
    }

    /// The version of the rules and the reply: a hash of every script's name and whether it is
    /// active, and of the text of the script holding them.
    pub(crate) fn version(&self) -> String {
        let mut listed: Vec<&ListedScript> = self.listed.iter().collect();
        listed.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let mut hash = Sha256::new();
        for script in listed {
            hash.update(script.name.as_str().as_bytes());
            hash.update([0, u8::from(script.active), 0]);
        }
        hash.update([1]);
        hash.update(self.text.as_bytes());
        hash.finalize()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// Whether the script holding the rules runs: it is active, or there is none yet, which a
    /// write creates running.
    pub(crate) fn running(&self) -> bool {
        self.script.as_ref().is_none_or(|script| script.active)
    }

    /// The name a write stores the rules under.
    pub(crate) fn target(&self) -> ScriptName {
        self.script.as_ref().map_or_else(
            || ScriptName::new(RULES_SCRIPT).expect("a valid name"),
            |script| script.name.clone(),
        )
    }
}

/// The script that holds the rules: the active one; else the engine's own `rules` script, or the
/// only one. `None` where there is none, or several and none of them active or named `rules`: the
/// engine cannot tell which the user means, and starts its own rather than pick one. The same
/// choice the JMAP adapter makes, which has a reply script to set aside and ManageSieve does not.
fn holding_rules(listed: &[ListedScript]) -> Option<&ListedScript> {
    listed
        .iter()
        .find(|script| script.active)
        .or_else(|| {
            listed
                .iter()
                .find(|script| script.name.as_str() == RULES_SCRIPT)
        })
        .or(match listed {
            [only] => Some(only),
            _ => None,
        })
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
