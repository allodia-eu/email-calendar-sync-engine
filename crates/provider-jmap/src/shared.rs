//! Shared-mailbox discovery: projecting the session's `accounts` map (RFC 8620 §1.6.2)
//! as neutral [`SharedMailbox`] entries.
//!
//! Neither verb issues a request. The map arrived with the session that
//! [`JmapClient::connect`](crate::JmapClient::connect) already fetched, which is why
//! enumeration is free on this protocol — and why the client used to throw the answer
//! away, keeping only `primaryAccounts`.

use engine_core::ids::{ProviderKey, SharedMailboxId};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};

use crate::{session::Session, session_accounts::SessionAccount};

/// Every account the session lists: the credential's own, plus each store shared with it.
pub(crate) fn list(session: &Session) -> Vec<SharedMailbox> {
    session.accounts().iter().map(project).collect()
}

/// Finds the account whose session `name` is `address`.
///
/// # Errors
///
/// [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) when no listed
/// account carries that name. There is no forbidden case to distinguish here: a JMAP
/// session lists precisely the accounts the credential may open, so anything missing from
/// it is absent rather than withheld.
pub(crate) fn resolve(session: &Session, address: &str) -> ProviderResult<SharedMailbox> {
    session
        .accounts()
        .iter()
        .find(|account| {
            account
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(address))
        })
        .map(project)
        .ok_or_else(|| {
            ProviderError::permanent(format!("the session lists no account named {address:?}"))
        })
}

/// Projects one session account entry as a neutral [`SharedMailbox`].
///
/// The handle is the JMAP account id — opaque, server-assigned, and the value that goes
/// straight back into [`JmapConfig::with_account`](crate::JmapConfig::with_account).
///
/// The entry's `name` becomes both the display name and the address, because RFC 8620
/// §1.6.2 says it is "usually the primary email address of the account" — *usually*, so it
/// is reported as-is rather than parsed or validated as one. A server that labels an
/// account with something else still yields a usable entry, whose handle is what
/// identifies it either way.
fn project(account: &SessionAccount) -> SharedMailbox {
    let label = account.name.clone().unwrap_or_else(|| account.id.clone());
    let mut mailbox = SharedMailbox::new(
        SharedMailboxId::new(
            ProviderKey::new(account.id.clone()).expect("a session account id is non-empty"),
        ),
        label.clone(),
    );
    // Only a real `name` is offered as an address; falling back to the opaque id would
    // hand a caller something that looks like one and is not.
    if account.name.is_some() {
        mailbox = mailbox.with_address(label);
    }
    if account.personal {
        mailbox = mailbox.as_personal();
    }
    mailbox
}
