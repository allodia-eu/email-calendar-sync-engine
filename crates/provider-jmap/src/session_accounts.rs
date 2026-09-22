//! The session's `accounts` map (RFC 8620 §1.6.2), and which of its entries a client's
//! method calls address.
//!
//! A session lists **every** account the credential can reach, not only its own: an
//! entry with `isPersonal: false` is one shared *with* the user. That map is the whole of
//! JMAP's shared-mailbox mechanism, and it costs nothing — it arrives in the document every
//! connect already fetches. On a server implementing JMAP Sharing (RFC 9670) it lists the
//! shares the user has subscribed to, so there a share appears once accepted.
//!
//! Binding a client to one of those accounts (`JmapConfig::with_shared_mailbox`) changes
//! exactly one thing: which account id each domain's calls carry. Unbound, a domain is
//! addressed at its `primaryAccounts` entry, as it always was. Bound, every domain the
//! bound account exposes is addressed at the bound account instead, and a domain it does
//! not expose is simply absent — so nothing else about the provider has to know it is
//! looking at someone else's mail, which is what "a shared mailbox is just another account"
//! has to mean in practice.
//!
//! What is deliberately *not* read off an entry for anything but the write capabilities is
//! `isReadOnly`. Live against Stalwart, a share whose only mailbox is read-only reports
//! `isReadOnly: false`, so the flag cannot say whether a given folder takes a write; the
//! folder's own `myRights` does (`crate::mail`).

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::JmapError;

/// One entry of the session's `accounts` map.
#[derive(Debug, Clone)]
pub(crate) struct SessionAccount {
    /// The server-assigned account id — what an `accountId` argument carries. Opaque, and
    /// not the engine's `AccountId`.
    pub(crate) id: String,
    /// The server's label for the account: "usually the primary email address"
    /// (RFC 8620 §1.6.2), and Stalwart's is exactly that — but a display string, never
    /// relied on as an address without checking its shape.
    pub(crate) name: Option<String>,
    /// Whether this is the user's own account (`isPersonal`).
    pub(crate) personal: bool,
    /// Whether the server declares the whole account read-only (`isReadOnly`).
    pub(crate) read_only: bool,
    /// The capability URNs the *account* advertises (`accountCapabilities`), distinct from
    /// the server-wide `capabilities`: a shared account need not expose every domain the
    /// server supports.
    capabilities: BTreeSet<String>,
}

impl SessionAccount {
    /// Whether the account advertises `urn` in its `accountCapabilities`.
    pub(crate) fn supports(&self, urn: &str) -> bool {
        self.capabilities.contains(urn)
    }
}

/// Parses the session's `accounts` map, or `None` when the session carries none.
///
/// An absent or malformed map is not an error: `primaryAccounts` is all the credential's
/// own mail needs, so such a server stays fully usable — it just cannot name any other
/// account. An entry keyed by the empty string is dropped for the same reason: no method
/// call could carry that id, and it is the one JSON key an opaque engine handle cannot hold.
pub(crate) fn parse_accounts(session: &Value) -> Option<Vec<SessionAccount>> {
    let map = session.get("accounts")?.as_object()?;
    let flag =
        |account: &Value, name: &str| account.get(name).and_then(Value::as_bool).unwrap_or(false);
    Some(
        map.iter()
            .filter(|(id, _)| !id.is_empty())
            .map(|(id, account)| SessionAccount {
                id: id.clone(),
                name: account
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                // RFC 8620 §1.6.2 makes both flags mandatory. Absent, read them the way
                // that promises least: "not the user's own", and — the RFC's own default
                // for `isReadOnly` — "writable".
                personal: flag(account, "isPersonal"),
                read_only: flag(account, "isReadOnly"),
                capabilities: account
                    .get("accountCapabilities")
                    .and_then(Value::as_object)
                    .map(|caps| caps.keys().cloned().collect())
                    .unwrap_or_default(),
            })
            .collect(),
    )
}

/// The account a bound client addresses, checked against what the session lists.
///
/// # Errors
///
/// [`JmapError::Session`] when the session lists no such account — a handle from another
/// server, or a share whose access has since been withdrawn. Failing here, at connect,
/// is the point: the alternative is a client that connects and then fails every call.
pub(crate) fn bound_account<'a>(
    accounts: &'a [SessionAccount],
    handle: &str,
) -> Result<&'a SessionAccount, JmapError> {
    accounts
        .iter()
        .find(|account| account.id == handle)
        .ok_or_else(|| {
            JmapError::session(format!(
                "the session lists no account {handle:?}; the credential may have lost access \
                 to the shared mailbox"
            ))
        })
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
                    "accountCapabilities": { capability::MAIL: {}, capability::CALENDARS: {} },
                },
                "d": {
                    "name": "bob@test.local",
                    // The live finding: a share whose only mailbox grants read alone still
                    // reports the *account* as writable.
                    "isPersonal": false,
                    "isReadOnly": false,
                    "accountCapabilities": { capability::MAIL: {} },
                },
                "": { "name": "nameless", "isPersonal": false },
            }
        })
    }

    #[test]
    fn every_entry_parses_with_its_own_capabilities() {
        let accounts = parse_accounts(&session()).expect("a map");
        // The empty-keyed entry is dropped: no call could address it.
        assert_eq!(accounts.len(), 2);
        let alice = bound_account(&accounts, "c").unwrap();
        assert!(alice.personal && !alice.read_only);
        assert_eq!(alice.name.as_deref(), Some("alice@test.local"));
        assert!(alice.supports(capability::MAIL) && alice.supports(capability::CALENDARS));

        let bob = bound_account(&accounts, "d").unwrap();
        assert!(!bob.personal);
        // Per-account capabilities are not the server's: this share exposes mail only.
        assert!(bob.supports(capability::MAIL) && !bob.supports(capability::CALENDARS));
    }

    #[test]
    fn a_session_without_an_accounts_map_names_no_account() {
        assert!(parse_accounts(&json!({})).is_none());
        assert!(parse_accounts(&json!({ "accounts": "nonsense" })).is_none());
    }

    #[test]
    fn missing_flags_read_as_the_weaker_promise() {
        let accounts = parse_accounts(&json!({ "accounts": { "x": {} } })).unwrap();
        assert!(!accounts[0].personal && !accounts[0].read_only);
        assert!(accounts[0].name.is_none());
        assert!(!accounts[0].supports(capability::MAIL));
    }

    #[test]
    fn a_handle_the_session_does_not_list_is_refused() {
        let accounts = parse_accounts(&session()).unwrap();
        let err = bound_account(&accounts, "zzz").unwrap_err();
        assert!(err.to_string().contains("lost access"), "{err}");
    }
}
