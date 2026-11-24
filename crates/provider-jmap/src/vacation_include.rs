//! The reply while the user's rules run: kept in the server's reply script, run by the rules
//! script's `include` (`crate::sieve_scripts`).
//!
//! On Stalwart the reply is a script of its own, and turning it on activates that script, which
//! switches the user's rules off (RFC 9661 §2.1). So while a script of the user's runs, the
//! reply is written as `VacationResponse/set` with `isEnabled: false`, which stores its words in
//! the reply's script and switches nothing, and then switched on or off by adding or removing
//! the rules script's `include` of it. A delivery test showed the included reply fires with
//! `isEnabled` false, which is why the include, not the flag, is what turns it off here.
//!
//! What is possible is decided before anything is sent ([`path`]). Where the running script
//! already says what the reply should, only the words are written. Adding the include needs the
//! condition (`jmap.md`, "the condition"): the account lists `include`, and the `SieveScript` and
//! `VacationResponse` states read in one response are one counter; where it fails, or the script
//! cannot take the include, the reply goes on with the server's own flag, and the receipt says
//! what that did to the rules. Taking the include out needs only the script, so turning the
//! reply off never answers success through the flag while the running script still includes it:
//! a script that cannot be read or rewritten then is refused before anything is written.
//!
//! Two requests, because Stalwart does not resolve a result reference for `ifInState`: it
//! ignored `#ifInState` pointing at the first call's `newState` even when that call failed
//! (`docs/spec-deviations.md`, STALWART-7). The second is guarded by the `newState` the first
//! answered, so nothing that landed between them is overwritten. If the first lands and the second
//! does not, the reply's words are stored and whether it runs is as it was: the write answers
//! [`requires_confirmation`](ProviderError::requires_confirmation), and the caller reads the
//! reply again before writing again.

use engine_core::{
    vacation::{RulesEffect, VacationReceipt},
    version::VacationVersion,
};
use engine_provider::{ProviderError, ProviderResult};
use serde_json::{Value, json};

use crate::{
    error::{JmapError, set_refusal},
    executor::Executor,
    request::{Request, capability},
    rules::{self, sieve_account},
    session_accounts::JmapAccountId,
    sieve_scripts::{self, ComposeError, Script, Scripts, includes_reply, reply_runs},
};

/// What the include path needs: the scripts listed, the running one, and its text.
pub(crate) struct Prepared {
    listing: rules::Listing,
    running: Script,
}

/// How a reply is written, decided before anything is sent.
pub(crate) enum Path {
    /// With the server's own flag, as without rules: no script of the user's runs, or the reply
    /// is to go on and the running script cannot take the include.
    Flag {
        /// Whether a script of the user's runs now, which turning the reply on by the flag
        /// switches off (RFC 9661 §2.1).
        rules_running: bool,
    },
    /// Through the running script: the words, then, where `toggled` is a script, the include
    /// added or taken out.
    Include {
        prepared: Prepared,
        toggled: Option<String>,
    },
}

/// How to write a reply that is to be `enabled` ([module docs](self)), decided before anything
/// is sent, so nothing is half-written for a reason known in advance.
///
/// - Where the running script already says what the reply should (it includes the reply, or not),
///   only the words are written.
/// - Turning the reply on adds the include where the condition holds and the script can take it;
///   otherwise the server's own flag, whose effect on the rules the receipt reports.
/// - Turning it off takes an include out, which needs only the script, not the shared counter: a
///   script that may include the reply and cannot be read, or cannot be rewritten without it, is
///   refused, because the flag would answer "off" while the include kept the reply running.
///
/// # Errors
///
/// The listing's or the download's own class; `item_unavailable` for a script that includes the
/// reply and cannot be rewritten without it.
pub(crate) async fn path(executor: &dyn Executor, enabled: bool) -> ProviderResult<Path> {
    let session = executor.session();
    if !rules::reachable(session) {
        return Ok(Path::Flag {
            rules_running: false,
        });
    }
    let listing = rules::list(executor).await?;
    let Some(running) = listing.scripts.rules_running().cloned() else {
        return Ok(Path::Flag {
            rules_running: false,
        });
    };
    let account = sieve_account(session)?;
    let text = match sieve_scripts::download(executor, account, &running.blob).await {
        Ok(text) => text,
        // It may include the reply: switching it off by the flag could leave it running.
        Err(err) if !enabled => return Err(err.into()),
        Err(_) => {
            return Ok(Path::Flag {
                rules_running: true,
            });
        }
    };
    let toggled = if includes_reply(&text) == enabled {
        None
    } else if enabled {
        if !listing.include_usable(session) {
            return Ok(Path::Flag {
                rules_running: true,
            });
        }
        match compose(&text, true) {
            Ok(text) => Some(text),
            Err(_) => {
                return Ok(Path::Flag {
                    rules_running: true,
                });
            }
        }
    } else {
        Some(compose(&text, false).map_err(|err| {
            ProviderError::item_unavailable(format!(
                "the rules script runs the reply and cannot be rewritten without it: {err}"
            ))
        })?)
    };
    Ok(Path::Include {
        prepared: Prepared { listing, running },
        toggled,
    })
}

/// The running script's text with the include for `on`.
fn compose(text: &str, on: bool) -> Result<String, ComposeError> {
    let user = sieve_scripts::user_rules(text);
    sieve_scripts::compose(text, &user, on)
}

/// Whether the reply runs through the running rules script, as `scripts` say: `false` where that
/// script cannot be read, so the reply reads as its own flag says.
pub(crate) async fn runs(executor: &dyn Executor, scripts: &Scripts) -> bool {
    let text = match scripts.rules_running() {
        Some(_) => match rules::rules_text(executor, scripts).await {
            Ok(text) => Some(text),
            Err(_) => return false,
        },
        None => None,
    };
    reply_runs(scripts, text.as_deref())
}

/// Writes the reply's words while the user's rules run ([module docs](self)), then, where
/// `toggled` is a script, the include.
pub(crate) async fn write(
    executor: &dyn Executor,
    vacation_account: &JmapAccountId,
    guard: Option<&VacationVersion>,
    prepared: &Prepared,
    toggled: Option<String>,
    body: Value,
) -> ProviderResult<VacationReceipt> {
    let Prepared { listing, running } = prepared;
    let scripts = &listing.scripts;
    // A reply read before the listing's state is stale: refused before anything is sent. (Where
    // the listing could not read the reply's state, the words' own `ifInState` refuses it.)
    if let (Some(version), Some(listed)) = (guard, listing.reply_state.as_deref())
        && version.as_str() != listed
    {
        return Err(ProviderError::conflict(
            "the out-of-office reply changed since it was read; read it again",
        ));
    }
    let account = sieve_account(executor.session())?;
    let blob = match &toggled {
        Some(text) => Some(sieve_scripts::upload(executor, account, text).await?),
        None => None,
    };

    // 1. The reply's words, switched off, so the server activates no script of its own.
    let mut update = body;
    update["isEnabled"] = json!(false);
    let mut arguments = json!({ "accountId": vacation_account, "update": { "singleton": update } });
    if let Some(version) = guard {
        arguments["ifInState"] = json!(version.as_str());
    }
    let mut request = Request::new([capability::CORE, capability::VACATION_RESPONSE]);
    let set = request.invoke("VacationResponse/set", arguments);
    let response = executor
        .execute(&request)
        .await
        .map_err(JmapError::after_write)?;
    let result = response.result(&set).map_err(JmapError::after_write)?;
    if let Some(refusal) = set_refusal(result, "notUpdated", "singleton") {
        return Err(JmapError::refused("singleton", refusal).into());
    }
    let state = |key| result.get(key).and_then(Value::as_str);
    let written = state("newState")
        .ok_or_else(|| JmapError::unanswered("singleton", "updated", "notUpdated"))?
        .to_owned();
    let Some(blob) = blob else {
        return Ok(VacationReceipt::new(
            Some(VacationVersion::new(written)),
            RulesEffect::Untouched,
        ));
    };
    let half_done = || {
        ProviderError::needs_confirmation(
            "the out-of-office reply's text was stored, but switching it on or off was not; \
             read it again before writing again",
        )
    };
    // 2. The include, guarded by the `SieveScript` state the script was composed at. Where the
    // words left from that very state, the two types are one counter (as on Stalwart), and the
    // words moved it: the guard is the state they left. Where they did not and the counters are
    // known to be one, something landed between the listing and the words (an unguarded write
    // does not check), and the script is not written over. Otherwise the counters are apart, and
    // the listed state guards it.
    let listed = scripts.state.as_str();
    let step_two = if state("oldState") == Some(listed) {
        written
    } else if listing.reply_state.as_deref() == Some(listed) {
        return Err(half_done());
    } else {
        listed.to_owned()
    };
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let set = request.invoke(
        "SieveScript/set",
        json!({
            "accountId": account,
            "ifInState": step_two,
            "update": { running.id.as_str(): { "blobId": blob.as_str() } },
        }),
    );
    let response = executor.execute(&request).await.map_err(|_| half_done())?;
    let result = response.result(&set).map_err(|_| half_done())?;
    if result
        .get("updated")
        .and_then(|updated| updated.get(running.id.as_str()))
        .is_none()
    {
        return Err(half_done());
    }
    let version = result
        .get("newState")
        .and_then(Value::as_str)
        .map(VacationVersion::new);
    Ok(VacationReceipt::new(version, RulesEffect::Untouched))
}

#[cfg(test)]
#[path = "vacation_include_tests.rs"]
mod tests;
