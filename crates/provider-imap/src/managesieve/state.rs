//! An account's Sieve scripts as a rule editor sees them: which script holds the rules (and, on
//! ManageSieve, the out-of-office reply), its text, the server's own reply script where one runs,
//! and the version a write is guarded by.
//!
//! The script is chosen as every adapter chooses it (`engine_sieve::dialect::scripts`). ManageSieve
//! tells a server's own reply script only by its text, a `vacation` no `# rule:[…]` name marks
//! (`unnamed_vacation`), so every listed script is read: one `GETSCRIPT` each, a handful on any
//! account. Such a script is never the rules'; where it runs, as the active one or through the
//! rules script's `include`, the rules are written beside it and it keeps running.
//!
//! ManageSieve keeps no version of a script and has no conditional `PUTSCRIPT`, so the version is
//! the engine's: a SHA-256 of the script list (each name and whether it is active) and the text
//! of the script that holds the rules. A write lists and reads them again in its own session and
//! refuses when the hash moved, which narrows the race with another client to the one round trip
//! between that read and `PUTSCRIPT`; it does not close it (`WriteGuard::Absent`).

use core::fmt::Write as _;

use engine_core::rules::MailRule;
pub(crate) use engine_sieve::dialect::scripts::RULES_SCRIPT;
use engine_sieve::dialect::{
    include::{included_scripts, includes_script, without_include},
    read_rules,
    reply::unnamed_vacation,
    scripts::{Choice, Listed, rules_script},
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    error::SieveResult,
    name::ScriptName,
    session::{ListedScript, Session},
};

/// The account's scripts as listed, the one that holds the rules, and its text.
pub(crate) struct State {
    pub(crate) listed: Vec<ListedScript>,
    /// The script holding the rules, or `None` where a write creates [`RULES_SCRIPT`].
    pub(crate) script: Option<ListedScript>,
    /// Its text; empty where there is none.
    pub(crate) text: String,
    /// A server's own reply script that runs: the active one, or one the rules script includes.
    pub(crate) server_reply: Option<ListedScript>,
}

impl State {
    /// Lists the scripts, reads each, and chooses the one that holds the rules.
    pub(crate) async fn read<S: AsyncRead + AsyncWrite + Unpin + Send>(
        session: &mut Session<S>,
    ) -> SieveResult<Self> {
        let listed = session.list_scripts().await?;
        let mut texts = Vec::with_capacity(listed.len());
        for script in &listed {
            texts.push(session.get_script(&script.name).await?);
        }
        let servers: Vec<bool> = texts.iter().map(|text| unnamed_vacation(text)).collect();
        let choice = rules_script(
            &listed
                .iter()
                .zip(&servers)
                .map(|(script, &server_reply)| Listed {
                    name: script.name.as_str(),
                    active: script.active,
                    server_reply,
                })
                .collect::<Vec<_>>(),
        );
        let (script, text) = match choice {
            Choice::Script(at) => (Some(listed[at].clone()), texts.swap_remove(at)),
            _ => (None, String::new()),
        };
        let included = included_scripts(&text);
        let server_reply = listed
            .iter()
            .zip(&servers)
            .filter(|(_, server)| **server)
            .map(|(script, _)| script)
            .find(|script| {
                script.active || included.iter().any(|name| name == script.name.as_str())
            })
            .cloned();
        Ok(Self {
            listed,
            script,
            text,
            server_reply,
        })
    }

    /// The user's rules: every rule of the script but the include of a server's reply script,
    /// which is the reply's (`engine_sieve::dialect::include`).
    pub(crate) fn rules(&self) -> Vec<MailRule> {
        match self.included_reply() {
            Some(name) => without_include(&self.text, name),
            None => read_rules(&self.text),
        }
    }

    /// The server's reply script the rules script includes, by name.
    pub(crate) fn included_reply(&self) -> Option<&str> {
        self.server_reply
            .as_ref()
            .map(|reply| reply.name.as_str())
            .filter(|name| includes_script(&self.text, name))
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

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
