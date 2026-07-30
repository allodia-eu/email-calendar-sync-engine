//! The session's `accounts` map (RFC 8620 §1.6.2) and which of its entries a client's
//! method calls address.
//!
//! A JMAP session lists **every** account the credential can access, not just its own:
//! `isPersonal: false` marks a store shared *with* the user. That map is the whole of
//! JMAP's shared-mailbox mechanism — free, already in the document every connect
//! fetches — and the client used to discard it, keeping only `primaryAccounts`.
//!
//! Two things live here as a result:
//!
//! - [`SessionAccount`], one parsed entry, which [`crate::JmapProvider`] hands back as an
//!   [`engine_provider::SharedMailbox`].
//! - the **selector**: which account id the four `Session::*_account_id()` accessors resolve to.
//!   Unselected, that is the `primaryAccounts` entry for the capability, as before. Selected, it is
//!   the named account — so binding a provider to a shared store changes nothing else about it,
//!   which is what "a shared mailbox is just another account" has to mean in practice.
//!
//! Note what is deliberately *not* read off an entry: `isReadOnly`. Live against
//! Stalwart, a share granting a single read-only mailbox reports `isReadOnly: false`, so
//! the flag cannot answer "may I write?" — the mailbox's own `myRights` does
//! (`engine_core::mail::MailboxAccess`). It is parsed only where RFC 8620 makes it
//! meaningful: gating the *write* capabilities the provider advertises, exactly as before
//! this file existed.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::JmapError;

/// One entry of the session's `accounts` map.
#[derive(Debug, Clone)]
pub(crate) struct SessionAccount {
    /// The server-assigned account id — the value that goes in an `accountId` argument.
    /// Not stable across servers, and not the engine's `AccountId`.
    pub(crate) id: String,
    /// The server's label for the account. RFC 8620 §1.6.2 says it is "usually the
    /// primary email address", and Stalwart's is exactly that, but it is a display
    /// string, not an address the engine may rely on.
    pub(crate) name: Option<String>,
    /// Whether this is the credential's **own** account (`isPersonal`).
    pub(crate) personal: bool,
    /// Whether the server declares the whole account read-only (`isReadOnly`).
    pub(crate) read_only: bool,
    /// The capability URNs the *account* advertises (`accountCapabilities`) — distinct
    /// from the server-wide `capabilities` map. A shared account need not expose every
    /// domain the server supports, so this is what says whether asking it for mail (or
    /// calendars, or contacts) can work at all.
    capabilities: BTreeSet<String>,
}

impl SessionAccount {
    /// Whether the account advertises `urn` in its `accountCapabilities`.
    pub(crate) fn supports(&self, urn: &str) -> bool {
        self.capabilities.contains(urn)
    }
}

/// Parses the session's `accounts` map, in the document's own order.
///
/// A malformed or absent map yields an empty list rather than an error: `primaryAccounts`
/// is what the sync path needs, and a server that omits `accounts` (against the RFC) is
/// still perfectly usable for the credential's own mail — it just cannot enumerate shares.
pub(crate) fn parse_accounts(session: &Value) -> Vec<SessionAccount> {
    let Some(map) = session.get("accounts").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .map(|(id, account)| SessionAccount {
            id: id.clone(),
            name: account
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            // RFC 8620 §1.6.2 makes both flags mandatory; default to the safer reading if
            // a server omits them — "not mine" and "writable", matching the RFC's own
            // default for `isReadOnly`.
            personal: account
                .get("isPersonal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            read_only: account
                .get("isReadOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            capabilities: account
                .get("accountCapabilities")
                .and_then(Value::as_object)
                .map(|caps| caps.keys().cloned().collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// Which account a capability's method calls address: the selected account when a
/// provider is bound to one, otherwise the `primaryAccounts` entry.
///
/// # Errors
///
/// [`JmapError::Session`] when neither is available, and — importantly — when the
/// **selected** account does not advertise `urn`. That is a local, actionable failure
/// ("this shared mailbox does not expose calendars") rather than a method error from a
/// server that was asked for something the session already said was not there.
pub(crate) fn resolve<'a>(
    accounts: &'a [SessionAccount],
    selected: Option<&str>,
    primary: Option<&'a str>,
    urn: &str,
    domain: &str,
) -> Result<&'a str, JmapError> {
    let Some(selected) = selected else {
        return primary.ok_or_else(|| JmapError::session(format!("no primary {domain} account")));
    };
    let account = accounts
        .iter()
        .find(|account| account.id == selected)
        .ok_or_else(|| {
            JmapError::session(format!("session lists no account {selected:?} to bind to"))
        })?;
    if !account.supports(urn) {
        return Err(JmapError::session(format!(
            "account {selected:?} does not expose {domain}"
        )));
    }
    Ok(&account.id)
}

/// Validates a selected account id against the session at parse time, so a stale or
/// foreign handle fails on connect rather than on the first method call.
///
/// # Errors
///
/// [`JmapError::Session`] if the session does not list `selected`.
pub(crate) fn validate_selection(
    accounts: &[SessionAccount],
    selected: Option<&str>,
) -> Result<(), JmapError> {
    let Some(selected) = selected else {
        return Ok(());
    };
    if accounts.iter().any(|account| account.id == selected) {
        return Ok(());
    }
    Err(JmapError::session(format!(
        "session lists no account {selected:?}; the credential may have lost access to it"
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::request::capability;

    fn session() -> Value {
        json!({
            "accounts": {
                "c": {
                    "name": "alice@test.local",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {
                        capability::MAIL: {},
                        capability::CALENDARS: {},
                    },
                },
                "d": {
                    "name": "bob@test.local",
                    // The live finding: a share whose only mailbox is read-only still
                    // reports the *account* as writable.
                    "isPersonal": false,
                    "isReadOnly": false,
                    "accountCapabilities": { capability::MAIL: {} },
                },
            }
        })
    }

    fn find<'a>(accounts: &'a [SessionAccount], id: &str) -> &'a SessionAccount {
        accounts.iter().find(|a| a.id == id).expect("account")
    }

    #[test]
    fn parses_every_entry_with_its_own_capabilities() {
        let accounts = parse_accounts(&session());
        assert_eq!(accounts.len(), 2);
        let alice = find(&accounts, "c");
        assert!(alice.personal && !alice.read_only);
        assert_eq!(alice.name.as_deref(), Some("alice@test.local"));
        assert!(alice.supports(capability::MAIL) && alice.supports(capability::CALENDARS));

        let bob = find(&accounts, "d");
        assert!(!bob.personal);
        // Per-account capabilities are not the server's: bob's share exposes mail only.
        assert!(bob.supports(capability::MAIL) && !bob.supports(capability::CALENDARS));
    }

    #[test]
    fn a_session_without_an_accounts_map_is_still_usable() {
        // Not an error: `primaryAccounts` is what the sync path needs, so a server that
        // omits `accounts` simply cannot enumerate shares.
        assert!(parse_accounts(&json!({})).is_empty());
        assert!(parse_accounts(&json!({ "accounts": "nonsense" })).is_empty());
    }

    #[test]
    fn unselected_resolution_uses_the_primary_account() {
        let accounts = parse_accounts(&session());
        assert_eq!(
            resolve(&accounts, None, Some("c"), capability::MAIL, "mail").unwrap(),
            "c"
        );
        assert!(resolve(&accounts, None, None, capability::MAIL, "mail").is_err());
    }

    #[test]
    fn a_selection_overrides_the_primary_but_must_expose_the_domain() {
        let accounts = parse_accounts(&session());
        // Bound to bob's share, mail calls address `d` even though `c` is primary.
        assert_eq!(
            resolve(&accounts, Some("d"), Some("c"), capability::MAIL, "mail").unwrap(),
            "d"
        );
        // Calendars on that share would be a method error on the wire; the session
        // already knows better, so it fails locally with a message naming the reason.
        let err = resolve(
            &accounts,
            Some("d"),
            Some("c"),
            capability::CALENDARS,
            "calendar",
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not expose calendar"));
    }

    #[test]
    fn a_selection_the_session_does_not_list_fails_at_connect() {
        let accounts = parse_accounts(&session());
        assert!(validate_selection(&accounts, None).is_ok());
        assert!(validate_selection(&accounts, Some("d")).is_ok());
        // A handle from another server, or one whose access was revoked.
        let err = validate_selection(&accounts, Some("zzz")).unwrap_err();
        assert!(err.to_string().contains("lists no account"));
        assert!(
            resolve(&accounts, Some("zzz"), Some("c"), capability::MAIL, "mail")
                .unwrap_err()
                .to_string()
                .contains("no account")
        );
    }
}
