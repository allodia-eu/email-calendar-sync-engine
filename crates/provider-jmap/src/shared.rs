//! Shared-account discovery: the session's `accounts` map, each entry asked what it holds,
//! projected as neutral [`SharedAccount`] entries.
//!
//! The map arrives with the session [`JmapClient::connect`](crate::JmapClient::connect)
//! already fetched, but it cannot be read alone. Every account lists its domains in
//! `accountCapabilities`, and on Stalwart that list overclaims in both directions
//! (STALWART-1, `docs/spec-deviations.md`): an account shared for one INBOX lists calendars
//! and refuses `Calendar/get`, one shared for one calendar lists mail and refuses
//! `Mailbox/get`. So each claimed domain is asked, with a `get` for no objects
//! (`ids: []`), and an `accountNotSupportedByMethod` answer means the domain is not there.
//! All the questions go in one request (more, only past the server's `maxCallsInRequest`).
//! Enumeration used to cost nothing; it now costs that request.
//!
//! Resolving an address needs nothing of its own: the trait's default reads this list
//! (`engine_provider::Provider`).

use engine_core::{
    ids::{ProviderKey, SharedAccountId},
    sync::SearchDomain,
};
use engine_provider::{SharedAccount, StoreDomains};
use serde_json::json;

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
    session_accounts::SessionAccount,
};

/// The domains a store is asked about: which the server must serve and the account claim,
/// and the `get` that answers for each.
const ASKED: [(SearchDomain, &str, &str); 2] = [
    (SearchDomain::Mail, capability::MAIL, "Mailbox/get"),
    (
        SearchDomain::Calendar,
        capability::CALENDARS,
        "Calendar/get",
    ),
];

/// The method errors that say an account does not hold a domain for this credential (RFC 8620
/// §3.6.2): it does not support the method, is not there, or the credential may not use the
/// method on it. Any other error fails the listing: a server fault is not an empty store, and
/// reading it as one would drop a store from the list a host shows on a blip.
const NOT_HELD: [&str; 3] = [
    "accountNotSupportedByMethod",
    "accountNotFound",
    "forbidden",
];

/// The stores shared with this credential: every account the session marks as not the
/// user's own (`isPersonal: false`), with the domains it actually answers for. An account
/// answering for none is left out.
///
/// The user's own account is left out too — a host is already holding it, and the list
/// reads as "add another account" only if it is not there to filter.
pub(crate) async fn list(executor: &dyn Executor) -> Result<Vec<SharedAccount>, JmapError> {
    let session = executor.session();
    let served = session.served();
    let mut questions = Vec::new();
    for account in session
        .accounts()
        .iter()
        .filter(|account| !account.personal)
    {
        for (domain, urn, method) in ASKED {
            if served.contains(domain) && account.supports(urn) {
                questions.push((account, domain, method));
            }
        }
    }
    let using = [capability::CORE].into_iter().chain(
        ASKED
            .iter()
            .filter(|(d, ..)| served.contains(*d))
            .map(|(_, urn, _)| *urn),
    );
    // In the order the session lists the accounts, which is the order a host shows them.
    let mut held: Vec<(&SessionAccount, StoreDomains)> = Vec::new();
    let per_request = session.limits().max_calls_in_request.max(1);
    for batch in questions.chunks(per_request) {
        let mut request = Request::new(using.clone());
        let calls: Vec<String> = batch
            .iter()
            .map(|(account, _, method)| {
                request.invoke(*method, json!({ "accountId": account.id, "ids": [] }))
            })
            .collect();
        let response = executor.execute(&request).await?;
        for ((account, domain, _), call) in batch.iter().zip(&calls) {
            match response.result(call) {
                Ok(_) => match held.iter_mut().find(|(known, _)| known.id == account.id) {
                    Some((_, domains)) => *domains = domains.with(*domain),
                    None => held.push((*account, StoreDomains::of(*domain))),
                },
                Err(JmapError::Method { error_type, .. })
                    if NOT_HELD.contains(&error_type.as_str()) => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(held
        .into_iter()
        .map(|(account, domains)| project(account, domains))
        .collect())
}

/// One account as a neutral entry, offering `domains`. Its handle is the JMAP account id —
/// opaque and server-assigned, and what
/// [`JmapConfig::with_shared_account`](crate::JmapConfig::with_shared_account) takes back.
///
/// RFC 8620 §1.6.2 calls the account `name` "usually the primary email address", so it is
/// reported as the address only when it has the shape of one; otherwise it is only a
/// label. An unnamed account is labelled by its id.
fn project(account: &SessionAccount, domains: StoreDomains) -> SharedAccount {
    let handle = SharedAccountId::new(
        ProviderKey::new(account.id.clone()).expect("an empty account id is dropped at parse"),
    );
    let label = account.name.clone().unwrap_or_else(|| account.id.clone());
    let store = SharedAccount::new(handle, label, domains);
    match account
        .name
        .as_deref()
        .filter(|name| looks_like_address(name))
    {
        Some(address) => store.with_address(address),
        None => store,
    }
}

/// Whether `text` is shaped like a single `local@domain` address.
pub(crate) fn looks_like_address(text: &str) -> bool {
    !text.chars().any(char::is_whitespace)
        && text.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
}

#[cfg(test)]
mod tests {
    use super::looks_like_address;

    #[test]
    fn only_an_address_shaped_name_is_offered_as_an_address() {
        assert!(looks_like_address("support@test.local"));
        for label in [
            "Support Team",
            "support",
            "@test.local",
            "a@b@c",
            "a b@test.local",
        ] {
            assert!(!looks_like_address(label), "{label}");
        }
    }
}
