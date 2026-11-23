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

use engine_core::{
    ids::BlobId,
    rules::{MailRule, RuleBody},
};
use engine_sieve::{
    Segment, SegmentKind,
    dialect::{DialectError, carried, read_rules, write_rules},
    segment, string,
};
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{CallId, Request},
    session_accounts::JmapAccountId,
};

/// The name of the script a server keeps the reply in.
///
/// Observed on Stalwart (v1.0.2: the first `VacationResponse/set` creates a script by this name),
/// not fixed by RFC 9661, which names none. `live_rules` pins it, so a server that renames it
/// turns a test red rather than silently losing replies.
pub(crate) const REPLY_SCRIPT: &str = "vacation";

/// The name of the script the engine creates when the account has no rules script to edit.
pub(crate) const RULES_SCRIPT: &str = "rules";

/// The command a rules script runs the reply with, written first. `:optional`, so a reply
/// script someone destroyed (Stalwart lets a client destroy it once inactive, STALWART-3) does
/// not fail delivery.
pub(crate) const INCLUDE_REPLY: &str = "include :personal :optional \"vacation\";\r\n";

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

    /// The script that holds the account's rules: the one running; else, where none of the
    /// user's runs (the reply's runs, or nothing does), the engine's own `rules` script, or the
    /// one script besides the reply's. `None`: the rules are empty, and a write creates
    /// [`RULES_SCRIPT`]. With several scripts and none of them running or named `rules`, the
    /// engine cannot tell which the user means, and starts its own rather than pick one.
    pub(crate) fn rules(&self) -> Option<&Script> {
        if let Some(running) = self.rules_running() {
            return Some(running);
        }
        let others: Vec<&Script> = self
            .list
            .iter()
            .filter(|script| script.name != REPLY_SCRIPT)
            .collect();
        others
            .iter()
            .find(|script| script.name == RULES_SCRIPT)
            .or(match others.as_slice() {
                [only] => Some(only),
                _ => None,
            })
            .copied()
    }
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

/// Whether a script's text includes the reply's script.
pub(crate) fn includes_reply(text: &str) -> bool {
    segment(text).is_ok_and(|segments| segments.iter().any(is_reply_include))
}

/// Whether `segment` is an `include` of the reply's script, however it is written.
fn is_reply_include(segment: &Segment<'_>) -> bool {
    if segment.kind() != SegmentKind::Command
        || !segment
            .tokens()
            .first()
            .is_some_and(|name| name.is_identifier("include"))
    {
        return false;
    }
    let mut strings = segment
        .tokens()
        .iter()
        .filter(|token| token.kind().is_string());
    let target = strings.next().and_then(string::value);
    target.as_deref() == Some(REPLY_SCRIPT) && strings.next().is_none()
}

/// Whether `rule` is the include of the reply's script, which is no rule of the user's.
pub(crate) fn is_reply_include_rule(rule: &MailRule) -> bool {
    match &rule.body {
        RuleBody::Opaque(raw) => {
            segment(raw.text()).is_ok_and(|segments| segments.iter().any(is_reply_include))
        }
        _ => false,
    }
}

/// The user's rules in a script's text: every rule but the include of the reply.
pub(crate) fn user_rules(text: &str) -> Vec<MailRule> {
    read_rules(text)
        .into_iter()
        .filter(|rule| !is_reply_include_rule(rule))
        .collect()
}

/// `rules` written over `base`, with the include of the reply first where `reply_on`, and
/// without it otherwise. An include `base` already has is copied as it was.
pub(crate) fn compose(
    base: &str,
    rules: &[MailRule],
    reply_on: bool,
) -> Result<String, DialectError> {
    let mut list = Vec::with_capacity(rules.len() + 1);
    if reply_on {
        let existing = read_rules(base).into_iter().find(is_reply_include_rule);
        // The include, carried with what it needs (`include`), so the writer requires it.
        let include = carried(INCLUDE_REPLY).map(MailRule::opaque);
        list.extend(existing.or(include));
    }
    list.extend(
        rules
            .iter()
            .filter(|rule| !is_reply_include_rule(rule))
            .cloned(),
    );
    write_rules(base, &list)
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
