//! The session's `accounts` map (RFC 8620 §1.6.2), and which of its entries a client's
//! method calls address.
//!
//! A session lists **every** account the credential can reach, not only its own: an
//! entry with `isPersonal: false` is one shared *with* the user. That map is the whole of
//! JMAP's shared-mailbox mechanism, and it costs nothing — it arrives in the document every
//! connect already fetches. On a server implementing JMAP Sharing (RFC 9670) it lists the
//! shares the user has subscribed to, so there a share appears once accepted.
//!
//! Binding a client to one of those accounts (`JmapConfig::with_shared_account`) changes
//! exactly one thing: which account id each domain's calls carry. Unbound, a domain is
//! addressed at its `primaryAccounts` entry, as it always was. Bound, every domain the
//! bound account exposes is addressed at the bound account instead, and a domain it does
//! not expose is simply absent — so nothing else about the provider has to know it is
//! looking at someone else's mail, which is what "a shared mailbox is just another account"
//! has to mean in practice.
//!
//! What is deliberately *not* read off an entry for anything but the write capabilities is
//! `isReadOnly`. Live against Stalwart v1.0.2, a share mixing a writable and a read-only folder
//! reports `isReadOnly: false` (v0.16 said `false` even when its only mailbox was read-only),
//! so the flag cannot say whether a given folder takes a write; the folder's own `myRights`
//! does (`crate::mail`).

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::{error::JmapError, request::capability, session::Session};

/// A JMAP account id (RFC 8620 §1.6.2): the server's name for an account, which every method
/// call's `accountId` argument carries.
///
/// The server assigns it, and the [`Session`] says which one each data domain is addressed at
/// ([`Session::mail_account_id`] and its siblings). It is not the engine's host-assigned
/// [`AccountId`](engine_core::ids::AccountId), and it is a type of its own so the two cannot be
/// mixed up. It serializes as the bare string, so it goes into a call's arguments as it is.
/// Some servers name an account by its user's login, so `Debug` shows the value only where
/// [`engine_core::redact::SHOWS_VALUES`] holds; `Display` and [`as_str`](Self::as_str) always do.
///
/// # Examples
///
/// ```no_run
/// use provider_jmap::{JmapClient, JmapError, Request, capability};
/// use serde_json::json;
///
/// fn mailboxes(client: &JmapClient) -> Result<Request, JmapError> {
///     let account = client.session().mail_account_id()?;
///     let mut request = Request::new([capability::CORE, capability::MAIL]);
///     request.invoke("Mailbox/get", json!({ "accountId": account }));
///     Ok(request)
/// }
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct JmapAccountId(String);

impl fmt::Debug for JmapAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JmapAccountId")
            .field(&engine_core::redact::Content(&self.0))
            .finish()
    }
}

impl JmapAccountId {
    /// An account id as a server names it, unchecked.
    ///
    /// Take the one the current [`Session`] answers where there is one: only the session says
    /// which account a call may address, and an id kept from an earlier session can go stale
    /// (a share withdrawn, an account renamed). This is for a host that already holds an id
    /// the server gave it, and for tests.
    ///
    /// # Examples
    ///
    /// ```
    /// use provider_jmap::JmapAccountId;
    ///
    /// let account = JmapAccountId::new("c");
    /// assert_eq!(account.as_str(), "c");
    /// assert_eq!(serde_json::to_value(&account)?, serde_json::json!("c"));
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as the server assigned it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JmapAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which account each capability's calls address: unbound, its `primaryAccounts` entry; bound to
/// a shared account, that account, and only for the capabilities it lists, since a share need
/// not carry every domain the server does.
#[derive(Debug, Clone)]
pub(crate) struct Addressing {
    primary: BTreeMap<String, JmapAccountId>,
    bound: Option<(JmapAccountId, BTreeSet<String>)>,
}

impl Addressing {
    pub(crate) fn new(primary: Option<&Value>, bound: Option<&SessionAccount>) -> Self {
        let primary = primary
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(urn, id)| Some((urn.clone(), JmapAccountId::new(id.as_str()?))))
                    .collect()
            })
            .unwrap_or_default();
        let bound = bound.map(|account| {
            (
                JmapAccountId::new(account.id.clone()),
                account.capabilities.clone(),
            )
        });
        Self { primary, bound }
    }

    pub(crate) fn account_for(&self, urn: &str) -> Option<&JmapAccountId> {
        match &self.bound {
            Some((id, capabilities)) => capabilities.contains(urn).then_some(id),
            None => self.primary.get(urn),
        }
    }

    /// The shared account this client is bound to, if any.
    pub(crate) fn bound(&self) -> Option<&JmapAccountId> {
        self.bound.as_ref().map(|(id, _)| id)
    }
}

/// Which account each capability's calls address.
impl Session {
    /// Whether the server lists `urn` among its `capabilities` (RFC 8620 §2): whether it speaks
    /// the extension at all. Whether a call for it can be made is
    /// [`account_for`](Self::account_for).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) {
    /// use provider_jmap::capability::SIEVE;
    /// let speaks_sieve = session.has_capability(SIEVE);
    /// # }
    /// ```
    #[must_use]
    pub fn has_capability(&self, urn: &str) -> bool {
        self.server_capabilities.contains(urn)
    }

    /// The account calls for capability `urn` address: the `primaryAccounts` entry for it, or,
    /// for a client bound to a shared account, that account where it lists `urn`. `None` where
    /// no call for `urn` can be addressed. The domain accessors below answer this for their
    /// own URN.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use provider_jmap::{JmapClient, JmapError, Request, capability};
    /// use serde_json::json;
    ///
    /// async fn sieve_scripts(client: &JmapClient) -> Result<Option<serde_json::Value>, JmapError> {
    ///     let Some(account) = client.session().account_for(capability::SIEVE) else {
    ///         return Ok(None);
    ///     };
    ///     let mut request = Request::new([capability::CORE, capability::SIEVE]);
    ///     let get = request.invoke("SieveScript/get", json!({ "accountId": account, "ids": null }));
    ///     let response = client.execute(&request).await?;
    ///     Ok(response.result(&get)?.get("list").cloned())
    /// }
    /// ```
    #[must_use]
    pub fn account_for(&self, urn: &str) -> Option<&JmapAccountId> {
        self.addressing.account_for(urn)
    }

    /// The account calls for extension `urn` address, where this client can make them at all:
    /// the server lists `urn` ([`has_capability`](Self::has_capability)) and a call for it is
    /// addressed ([`account_for`](Self::account_for)). The one lookup for an extension, so that
    /// no two callers decide "can I use it here" differently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) {
    /// use provider_jmap::capability::SIEVE;
    /// if let Some(account) = session.extension_account(SIEVE) {
    ///     println!("Sieve scripts live in account {account}");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn extension_account(&self, urn: &str) -> Option<&JmapAccountId> {
        if self.has_capability(urn) {
            self.account_for(urn)
        } else {
            None
        }
    }

    /// The account mail calls address: the bound account, or the `primaryAccounts` entry for
    /// mail.
    ///
    /// # Errors
    ///
    /// [`JmapError::Session`] when no account for mail is available: the server named no
    /// primary one, or the bound account does not expose mail.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) -> Result<(), provider_jmap::JmapError> {
    /// let account = session.mail_account_id()?;
    /// println!("mail lives in account {account}");
    /// # Ok(()) }
    /// ```
    pub fn mail_account_id(&self) -> Result<&JmapAccountId, JmapError> {
        self.addressed(self.account_for(capability::MAIL), "mail")
    }

    /// The account submission calls (`Identity`, `EmailSubmission`) address.
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for submission.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) -> Result<(), provider_jmap::JmapError> {
    /// let account = session.submission_account_id()?;
    /// # Ok(()) }
    /// ```
    pub fn submission_account_id(&self) -> Result<&JmapAccountId, JmapError> {
        self.addressed(self.account_for(capability::SUBMISSION), "submission")
    }

    /// The account calendar calls (`Calendar`, `CalendarEvent`) address.
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for calendars.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) -> Result<(), provider_jmap::JmapError> {
    /// let account = session.calendar_account_id()?;
    /// # Ok(()) }
    /// ```
    pub fn calendar_account_id(&self) -> Result<&JmapAccountId, JmapError> {
        self.addressed(self.account_for(capability::CALENDARS), "calendar")
    }

    /// The account address-book and contact-card calls address.
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for contacts.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(session: &provider_jmap::Session) -> Result<(), provider_jmap::JmapError> {
    /// let account = session.contact_account_id()?;
    /// # Ok(()) }
    /// ```
    pub fn contact_account_id(&self) -> Result<&JmapAccountId, JmapError> {
        self.addressed(self.account_for(capability::CONTACTS), "contacts")
    }

    /// Every account the session lists, the credential's own included; empty when it carries
    /// no `accounts` map.
    pub(crate) fn accounts(&self) -> &[SessionAccount] {
        self.accounts.as_deref().unwrap_or(&[])
    }

    /// The shared account this client is bound to, or `None` for the credential's own.
    pub(crate) fn bound_account(&self) -> Option<&SessionAccount> {
        let bound = self.addressing.bound()?;
        self.accounts()
            .iter()
            .find(|account| account.id == bound.as_str())
    }

    /// One domain's account id, or the error that says why there is none.
    fn addressed<'a>(
        &self,
        id: Option<&'a JmapAccountId>,
        domain: &str,
    ) -> Result<&'a JmapAccountId, JmapError> {
        id.ok_or_else(|| match self.addressing.bound() {
            Some(bound) => JmapError::session(format!(
                "the shared account {bound:?} this client is bound to does not expose {domain}"
            )),
            None => JmapError::session(format!("no primary {domain} account")),
        })
    }
}

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

/// Whether `account`, the account addressed for `urn`, offers it: its `accountCapabilities`
/// lists the URN. A server may support a domain that a given account lacks, and the account's
/// list is what says so (RFC 8620 §2). A capability with no account to address is not
/// offered, since every call for it is addressed to that account. An account object without
/// the (required) map is taken at the server's word.
pub(crate) fn account_offers(session: &Value, account: Option<&str>, urn: &str) -> bool {
    let Some(id) = account else {
        return false;
    };
    session
        .pointer(&format!("/accounts/{id}/accountCapabilities"))
        .and_then(Value::as_object)
        .is_none_or(|account_caps| account_caps.contains_key(urn))
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
    fn debug_shows_an_account_id_only_where_the_build_shows_values() {
        // Cyrus-style servers name an account by its user's login.
        let expected = if engine_core::redact::SHOWS_VALUES {
            r#"JmapAccountId("alice@test.local")"#
        } else {
            "JmapAccountId(<redacted, 16 bytes>)"
        };
        assert_eq!(
            format!("{:?}", JmapAccountId::new("alice@test.local")),
            expected
        );
    }

    #[test]
    fn a_handle_the_session_does_not_list_is_refused() {
        let accounts = parse_accounts(&session()).unwrap();
        let err = bound_account(&accounts, "zzz").unwrap_err();
        assert!(err.to_string().contains("lost access"), "{err}");
    }
}
