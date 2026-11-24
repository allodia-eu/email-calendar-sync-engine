//! An account's Sieve scripts as JMAP lists them (RFC 9661 §2), and the one answer to "does the
//! out-of-office reply run": shared by the rules (`crate::rules`) and the reply
//! (`crate::vacation`), so the two never decide it differently.
//!
//! On a server that runs the reply as a script of its own (RFC 9661 §4), an account runs one
//! script at a time (§2.1), so the reply's and the user's rules cannot both be active. The
//! engine keeps the rules script active and has it `include` the reply's script while the reply
//! is on; a delivery test on Stalwart showed the included `vacation` action fires and the rules
//! run (`docs/design/filters-and-out-of-office.md`, "Settled by live tests"). The include is
//! the rules script's first command, so no rule's `stop` or `discard` keeps the reply from
//! running.

use engine_core::{ids::BlobId, rules::MailRule};
use engine_sieve::dialect::{
    include,
    scripts::{Choice, Listed, rules_script},
};
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{CallId, Request},
    session::Session,
    session_accounts::JmapAccountId,
};

/// The name of the script a server keeps the reply in.
///
/// Observed on Stalwart (v1.0.2: the first `VacationResponse/set` creates a script by this name),
/// not fixed by RFC 9661, which names none. `live_rules` pins it, so a server that renames it
/// turns a test red rather than silently losing replies.
pub(crate) const REPLY_SCRIPT: &str = "vacation";

pub(crate) use engine_sieve::dialect::{include::ComposeError, scripts::RULES_SCRIPT};

/// One script as `SieveScript/get` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Script {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) blob: String,
}

/// The account's scripts, and the state they were listed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scripts {
    pub(crate) state: String,
    pub(crate) list: Vec<Script>,
}

/// Adds `SieveScript/get` for every script of `account` to `request`.
pub(crate) fn list_call(request: &mut Request, account: &JmapAccountId) -> CallId {
    request.invoke(
        "SieveScript/get",
        json!({ "accountId": account, "ids": null, "properties": ["name", "isActive", "blobId"] }),
    )
}

/// A `SieveScript/get` answer's scripts and state.
pub(crate) fn parse(result: &Value) -> Result<Scripts, JmapError> {
    let malformed = || JmapError::protocol("a SieveScript/get answer this client cannot read");
    let state = result
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let list = result
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?
        .iter()
        .map(|script| {
            let text = |name: &str| script.get(name).and_then(Value::as_str).map(str::to_owned);
            Some(Script {
                id: text("id")?,
                name: text("name")?,
                active: script.get("isActive")?.as_bool()?,
                blob: text("blobId")?,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(malformed)?;
    Ok(Scripts {
        state: state.to_owned(),
        list,
    })
}

impl Scripts {
    /// The reply's script, where the server keeps one.
    pub(crate) fn reply(&self) -> Option<&Script> {
        self.list.iter().find(|script| script.name == REPLY_SCRIPT)
    }

    /// The active script, where it holds the user's rules rather than the reply.
    pub(crate) fn rules_running(&self) -> Option<&Script> {
        self.list
            .iter()
            .find(|script| script.active && script.name != REPLY_SCRIPT)
    }

    /// The script that holds the account's rules, as every adapter chooses it
    /// (`engine_sieve::dialect::scripts::rules_script`), the reply's script known by its name.
    /// `None`: the rules are empty, and a write creates [`RULES_SCRIPT`].
    pub(crate) fn rules(&self) -> Option<&Script> {
        let listed: Vec<Listed<'_>> = self
            .list
            .iter()
            .map(|script| Listed {
                name: &script.name,
                active: script.active,
                server_reply: script.name == REPLY_SCRIPT,
            })
            .collect();
        match rules_script(&listed) {
            Choice::Script(at) => self.list.get(at),
            _ => None,
        }
    }
}

/// Whether the engine may run the reply through the rules script's `include`, on what this
/// response shows: the account lists `include`, and `state`, the `SieveScript` state, is the
/// `VacationResponse` state `reply_state` read in the same response, so the two are one counter
/// and the reply's words live in a script the rules can include. Both hold on Stalwart; where
/// either does not, the reply is written with the server's own flag (`jmap.md`, "the
/// condition").
pub(crate) fn include_usable(session: &Session, state: &str, reply_state: Option<&str>) -> bool {
    let lists_include = session.sieve.as_ref().is_some_and(|sieve| {
        sieve
            .extensions
            .iter()
            .any(|extension| extension.eq_ignore_ascii_case("include"))
    });
    lists_include && reply_state == Some(state)
}

/// Whether the reply runs: the server keeps it in a script, and that script is active, or the
/// active rules script, whose text is `running_text`, includes it. The one answer the reply's
/// read and the rules' write both use.
pub(crate) fn reply_runs(scripts: &Scripts, running_text: Option<&str>) -> bool {
    let Some(reply) = scripts.reply() else {
        return false;
    };
    reply.active || (scripts.rules_running().is_some() && running_text.is_some_and(includes_reply))
}

/// Whether a script's text includes the reply's script (`engine_sieve::dialect::include`, one
/// answer for every adapter).
pub(crate) fn includes_reply(text: &str) -> bool {
    include::includes_script(text, REPLY_SCRIPT)
}

/// The user's rules in a script's text: every rule but the include of the reply.
pub(crate) fn user_rules(text: &str) -> Vec<MailRule> {
    include::without_include(text, REPLY_SCRIPT)
}

/// `rules` written over `base`, with the include of the reply first where `reply_on`, and
/// without it otherwise. An include `base` already has is copied as it was; `:optional`, so a
/// reply script someone destroyed (Stalwart lets a client destroy it once inactive, STALWART-3)
/// does not fail delivery.
pub(crate) fn compose(
    base: &str,
    rules: &[MailRule],
    reply_on: bool,
) -> Result<String, ComposeError> {
    include::compose(base, rules, reply_on.then_some(REPLY_SCRIPT))
}

/// A script's text, downloaded by its blob id.
pub(crate) async fn download(
    executor: &dyn Executor,
    account: &JmapAccountId,
    blob: &str,
) -> Result<String, JmapError> {
    let template = executor
        .session()
        .download_url()
        .ok_or_else(|| JmapError::session("server advertised no downloadUrl"))?;
    let url = crate::blob::fill_download(
        template,
        account.as_str(),
        blob,
        "application/sieve",
        "script.sieve",
    );
    let bytes = executor.download(&url).await?;
    // RFC 5228 §2.1 lets a script hold other octets; such a script cannot be carried as text,
    // so it is refused rather than altered.
    String::from_utf8(bytes).map_err(|_| JmapError::protocol("a Sieve script that is not UTF-8"))
}

/// Uploads a script's text, answering its blob id (RFC 9661 §2.2: the upload URL).
pub(crate) async fn upload(
    executor: &dyn Executor,
    account: &JmapAccountId,
    text: &str,
) -> Result<BlobId, JmapError> {
    let template = executor
        .session()
        .upload_url()
        .ok_or_else(|| JmapError::session("server advertised no uploadUrl"))?;
    let url = crate::blob::fill_upload(template, account.as_str());
    executor
        .upload(&url, "application/sieve", text.as_bytes())
        .await
}

#[cfg(test)]
#[path = "sieve_scripts_tests.rs"]
mod tests;
