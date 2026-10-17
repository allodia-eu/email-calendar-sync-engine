//! Checking that a delegated credential can open a named mailbox.
//!
//! Graph has **no list route** for this: nothing answers "which mailboxes have been shared
//! with me". `GET /users/{address}/…` either works or it does not, so the adapter
//! advertises [`SharedMailboxes::ByAddress`](engine_provider::SharedMailboxes::ByAddress)
//! and a host asks the user to type an address.
//!
//! The probe is the mailbox's Inbox, asking for its id alone. A mail-folder read,
//! deliberately: it needs only the `Mail.Read*.Shared` scope a sync of that mailbox needs
//! anyway, and it tests the permission onboarding actually depends on. There is no
//! mailbox-*kind* lookup — `mailboxSettings`, which carries Graph's `userPurpose`, answers
//! `403` on every `/users/{other}/…` route for a delegated credential even with Full Access
//! (`graph.md`), so a second call could only ever fail.
//!
//! # What the answers look like (probed, not inferred)
//!
//! | Status | `code` | The address is… |
//! |---|---|---|
//! | 200 | — | a mailbox this credential can open |
//! | 404 | `ErrorInvalidUser` | not a principal the tenant knows |
//! | 404 | `MailboxNotEnabledForRESTAPI` | a principal whose mailbox is inactive, soft-deleted, or on-premises |
//! | 404 | `ErrorItemNotFound` (`Default folder Inbox not found`) | a principal whose Inbox the caller cannot see — #89 recorded it for a group |
//!
//! All three `404`s mean *you cannot open this as a mailbox*. What is absent matters as much:
//! #89 probed every mailbox of a real tenant and got no `403` on this route at all, so a
//! mailbox that exists and was never shared with the caller is a `404` too — Graph will not
//! disclose that it is there. So `Permanent` here means **not resolvable by you**, never
//! "does not exist". A `403 ErrorAccessDenied` is the
//! credential's *grant* falling short of the route (captured on `mailboxSettings` with the
//! scope granted), which a re-consent fixes, so it classifies `Authentication`.
//!
//! # The caller's own mailbox is not a shared one
//!
//! `GET /users/{own address}/…` answers `200` — as does any alias of it — and a host would
//! then onboard the signed-in mailbox a second time under another account. The address
//! cannot be compared to catch that, because an alias is a different string for the same
//! mailbox. The **Inbox** can: its id was observed identical whether reached through `/me`,
//! `/users/{upn}` or `/users/{address in another case}`, and different for a shared mailbox.
//! So the resolver reads the caller's own Inbox id too and refuses a match.

use engine_core::ids::{ProviderKey, SharedMailboxId};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};
use serde_json::Value;

use crate::{error::GraphError, principal::MailboxPrincipal, transport::GraphClient};

/// The probe path under a mailbox root: its Inbox, and nothing but the id — the status is
/// the answer, and the id is what the own-mailbox check compares.
const PROBE: &str = "/mailFolders/inbox?$select=id";

/// Resolves `address` to a mailbox this credential can open that is not its own.
///
/// # Errors
///
/// - An address that cannot safely go in a URL → `Permanent`, before any request
///   ([`MailboxPrincipal::user`]).
/// - Any of the three `404`s (module docs) → `Permanent`: not a mailbox this credential can open.
/// - `403` → `Authentication`: the credential's grant does not cover the route.
/// - The caller's own mailbox, by whatever address → `Permanent`.
/// - Anything else keeps its own class, so a throttle stays rate-limited and an outage retryable —
///   neither is ever reported as "no such mailbox".
pub(crate) async fn resolve(client: &GraphClient, address: &str) -> ProviderResult<SharedMailbox> {
    let principal = MailboxPrincipal::user(address)?;
    let probed = client
        .get(&client.principal_url(&principal, PROBE))
        .await
        .map_err(|err| classify(err, address))?;
    if let Some(own) = own_inbox_id(client).await?
        && inbox_id(&probed)? == own
    {
        return Err(ProviderError::permanent(format!(
            "{address:?} is this credential's own mailbox, not one shared with it"
        )));
    }
    let handle = SharedMailboxId::new(
        ProviderKey::new(address.to_owned()).expect("a validated address is never empty"),
    );
    Ok(SharedMailbox::new(handle, address).with_address(address))
}

/// The signed-in user's own Inbox id, or `None` when the credential has no mailbox of its
/// own (a licensed user without Exchange Online answers `404`) — in which case nothing it
/// resolves can be its own.
async fn own_inbox_id(client: &GraphClient) -> ProviderResult<Option<String>> {
    match client
        .get(&client.principal_url(&MailboxPrincipal::Me, PROBE))
        .await
    {
        Ok(own) => Ok(Some(inbox_id(&own)?)),
        Err(GraphError::Status { status: 404, .. }) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// The `id` of a probe answer.
fn inbox_id(folder: &Value) -> Result<String, GraphError> {
    folder
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GraphError::protocol("mail folder answer carries no id"))
}

/// Maps a failed probe onto the contract [`resolve`] documents.
fn classify(err: GraphError, address: &str) -> ProviderError {
    let GraphError::Status { status, code, .. } = &err else {
        // A transport failure or an unreadable body says nothing about the mailbox, so it
        // keeps its own class.
        return err.into();
    };
    match status {
        // Deliberately not "does not exist": one of these is a mailbox that is there and
        // simply not shared, and Graph will not say which.
        404 => ProviderError::permanent(format!(
            "no mailbox at {address:?} that this credential can open ({code:?})"
        ))
        .with_source(err),
        403 => ProviderError::authentication(format!(
            "this credential's grant does not cover the mailbox at {address:?} ({code:?}); \
             it needs consent to the shared-mailbox scopes"
        ))
        .with_source(err),
        _ => err.into(),
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
