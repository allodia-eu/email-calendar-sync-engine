//! Verifying that a delegated credential can open a named mailbox.
//!
//! Graph has **no list API** for this. There is no route that answers "which mailboxes have
//! been shared with me": `GET /users/{address}/…` either works or fails, so the adapter
//! advertises [`SharedMailboxes::ByAddress`](engine_provider::SharedMailboxes::ByAddress)
//! and the host asks the user to type an address.
//!
//! The probe is **one** request, and which one matters:
//!
//! - It is a **mail folder** `GET`. That needs only the `Mail.Read*.Shared` scope the sync already
//!   uses, and it tests the permission that actually matters — being able to read the mailbox's
//!   folders is precisely what onboarding it requires.
//! - There is deliberately **no mailbox-kind lookup**. `mailboxSettings` carries Graph's
//!   `userPurpose` (the shared-vs-user-vs-room discriminator) and is unreachable for
//!   another mailbox on a delegated credential: with `MailboxSettings.ReadWrite` granted,
//!   `/me/mailboxSettings/…` answers `200` while **every** `/users/{other}/mailboxSettings`
//!   route answers `403 ErrorAccessDenied` — whole object and each sub-path — even with
//!   Full Access to that mailbox. Verified live, and independently reported as
//!   [OfficeDev/office-js#6057](https://github.com/OfficeDev/office-js/issues/6057), open
//!   and unanswered by Microsoft since 2025-09-01; the best available explanation is that
//!   delegated `mailboxSettings` is signed-in-user-only *by design*. So a second call could
//!   only ever `403`, and there is no mailbox kind in the engine's model (`graph.md`).
//!
//! ## What the failures actually look like (probed, not inferred)
//!
//! Probing every mailbox of a real tenant produced **three** distinct `404` codes and no
//! `403` at all:
//!
//! | Status | `code` | The mailbox is… |
//! |---|---|---|
//! | 404 | `ErrorInvalidUser` | not a principal the tenant knows |
//! | 404 | `MailboxNotEnabledForRESTAPI` | a principal whose mailbox is inactive, soft-deleted, or on-premises |
//! | 404 | `ErrorItemNotFound` (`Default folder Inbox not found`) | reachable, but has no Inbox — a group or distribution list rather than a mailbox |
//!
//! All three mean *you cannot open this as a mailbox*, so all three classify the same. The
//! important consequence is what is **absent**: Graph does not answer `403` for a mailbox
//! that exists but has not been shared with the caller — it answers `404`, refusing to
//! disclose that the mailbox is there. So the resolver cannot tell "not shared with you"
//! from "does not exist", and that is a property of Graph rather than a gap here.
//!
//! `403 ErrorAccessDenied` is a different failure: the credential's **grant** does not cover
//! the route. It was captured on `/users/{other}/mailboxSettings` — 403 with
//! `MailboxSettings.ReadWrite` granted, while `/me/mailboxSettings` answers 200 — and a
//! probe made without `Mail.Read.Shared` would fail the same way. Hence the classification
//! below: 403 says *fix the grant*, 404 says *there is nothing here for you*.

use engine_core::ids::{ProviderKey, SharedMailboxId};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};

use crate::{
    error::GraphError,
    principal::{self, MailboxPrincipal},
    transport::GraphClient,
};

/// The probe: the mailbox's Inbox, asking for nothing but its id.
///
/// `$select=id` keeps the response to the smallest thing that proves access — the point is
/// the status code, not the payload.
const PROBE_PATH: &str = "/mailFolders/inbox?$select=id";

/// Resolves `address` to a mailbox this credential can open.
///
/// # Errors
///
/// - **Not a usable address** →
///   [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent), refused before any
///   request (see [`principal::validate_address`]).
/// - **Cannot be opened as a mailbox** (`404`, in any of its three codes — see the module docs) →
///   [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent): nothing about the
///   credential would make it resolve. This covers a mailbox that exists but has not been shared
///   with the caller too, because Graph will not say so.
/// - **The credential's grant does not cover the request** (`403 ErrorAccessDenied`) →
///   [`FailureClass::Authentication`](engine_core::error::FailureClass::Authentication): a
///   re-consent for the missing delegated scope is what would make it succeed.
/// - Anything else keeps its own classification — a `429` stays rate-limited and a `5xx` retryable,
///   so a transient failure is never reported as "no such mailbox".
pub(crate) async fn resolve(client: &GraphClient, address: &str) -> ProviderResult<SharedMailbox> {
    // Before anything reaches a URL. Percent-encoding is *not* enough on Graph: it decodes
    // the segment and re-resolves the path, so `../me` — which encodes to the harmless-looking
    // `..%2Fme` — was observed answering 200 with the signed-in user's **own** Inbox. Left
    // unchecked, a host would onboard its own mailbox believing it had onboarded someone
    // else's (`crate::principal::validate_address`).
    principal::validate_address(address).map_err(ProviderError::permanent)?;
    let principal = MailboxPrincipal::user(address);
    let url = client.principal_url(&principal, PROBE_PATH);
    match client.get(&url).await {
        Ok(_) => Ok(shared_mailbox(address)),
        Err(err) => Err(classify(err, address)),
    }
}

/// The neutral entry for a resolved mailbox.
///
/// The handle is the address itself, because on Graph that *is* the reopening handle — it is
/// what `/users/{…}` takes. It is still not identity: an alias resolves to its target
/// mailbox, so two addresses can name one store, and a host that stored the alias reopens
/// the same mailbox rather than a different one.
fn shared_mailbox(address: &str) -> SharedMailbox {
    let handle = SharedMailboxId::new(
        ProviderKey::new(address.to_owned()).expect("a resolved address is non-empty"),
    );
    SharedMailbox::new(handle, address.to_owned()).with_address(address.to_owned())
}

/// Maps the probe's failure onto the contract above.
fn classify(err: GraphError, address: &str) -> ProviderError {
    let (status, code) = match &err {
        GraphError::Status { status, code, .. } => (*status, code.as_deref()),
        // A transport failure, or a body that was not the JSON Graph promises: neither says
        // anything about whether the mailbox exists, so it keeps its own class.
        _ => return err.into(),
    };
    match status {
        // Deliberately not "no such mailbox": one of these codes means the mailbox is there
        // and simply not shared, and Graph does not say which. The message says what is
        // actually known.
        404 => ProviderError::permanent(format!(
            "no mailbox at {address:?} that this credential can open ({code:?})"
        ))
        .with_source(err),
        403 => ProviderError::authentication(format!(
            "this credential's grant does not cover the mailbox at {address:?} ({code:?}); \
             consent to the shared-mail scopes is needed"
        ))
        .with_source(err),
        _ => err.into(),
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
