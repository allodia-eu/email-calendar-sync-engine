//! Shared-mailbox discovery: the session's `accounts` map, projected as neutral
//! [`SharedMailbox`] entries.
//!
//! No request is issued. The map arrived with the session
//! [`JmapClient::connect`](crate::JmapClient::connect) already fetched, which is why
//! enumeration is free on this protocol. Resolving an address needs nothing of its own
//! either: the trait's default reads this list (`engine_provider::Provider`).

use engine_core::ids::{ProviderKey, SharedMailboxId};
use engine_provider::SharedMailbox;

use crate::{request::capability, session::Session, session_accounts::SessionAccount};

/// The mailboxes shared with this credential: every account the session marks as not the
/// user's own (`isPersonal: false`) that exposes mail.
///
/// The user's own account is left out — a host is already holding it, and the list reads
/// as "add another mailbox" only if it is not there to filter. An account shared for
/// another domain alone (a calendar, an address book) is left out too, because it is not
/// a mailbox and binding a mail client to it could sync nothing.
pub(crate) fn list(session: &Session) -> Vec<SharedMailbox> {
    session
        .accounts()
        .iter()
        .filter(|account| !account.personal && account.supports(capability::MAIL))
        .map(project)
        .collect()
}

/// One account as a neutral entry. Its handle is the JMAP account id — opaque and
/// server-assigned, and what
/// [`JmapConfig::with_shared_mailbox`](crate::JmapConfig::with_shared_mailbox) takes back.
///
/// RFC 8620 §1.6.2 calls the account `name` "usually the primary email address", so it is
/// reported as the address only when it has the shape of one; otherwise it is only a
/// label. An unnamed account is labelled by its id.
fn project(account: &SessionAccount) -> SharedMailbox {
    let handle = SharedMailboxId::new(
        ProviderKey::new(account.id.clone()).expect("an empty account id is dropped at parse"),
    );
    let label = account.name.clone().unwrap_or_else(|| account.id.clone());
    let mailbox = SharedMailbox::new(handle, label);
    match account
        .name
        .as_deref()
        .filter(|name| looks_like_address(name))
    {
        Some(address) => mailbox.with_address(address),
        None => mailbox,
    }
}

/// Whether `text` is shaped like a single `local@domain` address.
fn looks_like_address(text: &str) -> bool {
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
