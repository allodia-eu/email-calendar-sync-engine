//! Finding the mail stores a credential may open besides its own.
//!
//! A Microsoft 365 shared mailbox has no credentials of its own: a user is *granted
//! access* and their client opens it by address. That is not a Microsoft concept. Three
//! of the four mail protocols this engine speaks have a mechanism for it, and they differ
//! in the one way a caller has to plan around — whether the server will **list** the
//! stores, or only **answer for a named one**. [`SharedMailboxes`] is that difference and
//! nothing more.
//!
//! What this seam deliberately does *not* add is a scope, a cursor, or a second kind of
//! account. **A shared mailbox is just another account** (`providers.md`): the host
//! onboards it under an [`AccountId`](engine_core::ids::AccountId) of its own, binds a
//! provider to it with the same credential, and every sync, store and search path applies
//! unchanged. What was missing was the onboarding step — *which stores can I open?* —
//! and that is all [`Provider::list_shared_mailboxes`](crate::Provider::list_shared_mailboxes)
//! and [`Provider::resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox) answer.
//! Neither takes an account: like [`connection_info`](crate::Provider::connection_info), what
//! a credential can reach is a fact about the credential.
//!
//! **Binding** stays per adapter, because what a handle *is* differs per protocol and a URL
//! or path detail must not leak into a neutral type. Each adapter takes back the
//! [`SharedMailboxId`] it handed out through a method of the same name:
//! `JmapConfig::with_shared_mailbox`, `ImapConfig::with_shared_mailbox`, and Graph's
//! `MailboxPrincipal::shared`.
//!
//! # What a [`SharedMailbox`] does not carry
//!
//! - **Rights.** An account-level "read-only" is not a usable signal — live against Stalwart, a
//!   JMAP account shared read-only reports `isReadOnly: false` while its only mailbox grants `lr` —
//!   so rights are carried per folder, on [`Mailbox::access`](engine_core::mail::Mailbox::access),
//!   once the store is bound and synced.
//! - **Kind** (shared vs. user vs. room). Graph's `userPurpose` is the only such vocabulary any
//!   adapter here publishes, and a delegated credential cannot read it for any mailbox but its own
//!   — every `/users/{other}/mailboxSettings` route answers `403` even with Full Access
//!   (`graph.md`). A field that is always unknown for the stores it would describe is not worth a
//!   slot.
//! - **Whether it is the credential's own store.** It never is: the list excludes the store the
//!   credential opens by default, so a host renders it as "add another mailbox" without filtering.
//!   IMAP could not report its own store anyway — the personal namespace's prefix is the empty
//!   string, which is no handle.

use engine_core::ids::SharedMailboxId;

use crate::{Capabilities, Provider, ProviderError, ProviderResult, error::unsupported};

/// How an adapter can find the mail stores its credential may open besides its own.
///
/// Read it off [`Capabilities::shared_mailboxes`] **before** offering an onboarding flow:
/// it decides whether a host can offer a list to pick from or has to ask for an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedMailboxes {
    /// No mechanism, for this credential on this transport. Both verbs reject.
    ///
    /// Gmail: delegation is real but API-reachable only as `users/{userId}` with a service
    /// account holding domain-wide delegation, never with a user's own token (`google.md`).
    /// IMAP: a server without both `NAMESPACE` (RFC 2342) and `ACL` (RFC 4314), since a server
    /// with no ACLs has no way to share.
    #[default]
    Unsupported,
    /// The server lists the stores, so **both** verbs answer:
    /// [`list_shared_mailboxes`](crate::Provider::list_shared_mailboxes), and
    /// [`resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox) matching an address
    /// against that list.
    ///
    /// JMAP: the session's `accounts` map (RFC 8620 §1.6.2), which a server implementing JMAP
    /// Sharing fills with the shares the user has **subscribed** to (RFC 9670) — so on such
    /// a server a share appears here once accepted, not the moment it is granted. IMAP: the
    /// other-users' and shared namespaces (RFC 2342), one store per owner under each.
    Enumerable,
    /// The server will not list them but answers for a **named address**, so only
    /// [`resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox) answers.
    ///
    /// Microsoft Graph: no route lists the mailboxes a delegated credential has been granted;
    /// `GET /users/{address}/…` either works or it does not (`graph.md`).
    ByAddress,
}

/// One mail store a credential can open.
///
/// `#[non_exhaustive]`, so a later protocol can report a fact none of today's three do;
/// build one with [`SharedMailbox::new`] and [`with_address`](SharedMailbox::with_address).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedMailbox {
    /// The handle that binds a provider to this store — hand it back to the adapter that
    /// produced it. A JMAP account id, an IMAP namespace path, a Graph address.
    pub handle: SharedMailboxId,
    /// A label for the store: the server's own where it has one, otherwise the best the
    /// adapter can offer (an IMAP store is named by the path component that holds it).
    pub name: String,
    /// The address the server gives for this store, when what it gives has the shape of
    /// one.
    ///
    /// **Not identity.** A Microsoft 365 alias resolves to its target mailbox, so two
    /// addresses can name one store; the [`handle`](Self::handle) is what identifies it.
    /// `None` where the protocol names the store by something that is not an address — an
    /// IMAP server whose logins are bare user names.
    pub address: Option<String>,
}

impl SharedMailbox {
    /// A store that `handle` binds to, labelled `name`, with no address.
    #[must_use]
    pub fn new(handle: SharedMailboxId, name: impl Into<String>) -> Self {
        Self {
            handle,
            name: name.into(),
            address: None,
        }
    }

    /// Records the address the server gives for this store.
    #[must_use]
    pub fn with_address(mut self, address: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Whether this store answers to `address`: the domain half of an address is
    /// case-insensitive by definition (RFC 5321 §2.3.11) and no mail system distinguishes
    /// the local part in practice, so neither is distinguished here. ASCII only, because
    /// no case mapping is defined for an internationalized local part.
    #[must_use]
    pub fn answers_to(&self, address: &str) -> bool {
        self.address
            .as_deref()
            .is_some_and(|own| own.eq_ignore_ascii_case(address))
    }
}

impl Capabilities {
    /// States how the credential can find mail stores besides its own.
    ///
    /// Independent of every other capability, [`with_mail`](Self::with_mail) included: it
    /// is a property of the **credential**, so an adapter advertises it whether or not the
    /// host has onboarded any shared store — or is using this provider for one.
    #[must_use]
    pub const fn with_shared_mailboxes(mut self, mechanism: SharedMailboxes) -> Self {
        self.shared_mailboxes = mechanism;
        self
    }

    /// How the credential can find mail stores besides its own:
    /// [`Unsupported`](SharedMailboxes::Unsupported) unless the adapter says otherwise.
    #[must_use]
    pub const fn shared_mailboxes(self) -> SharedMailboxes {
        self.shared_mailboxes
    }
}

/// The body of the default
/// [`resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox): on an adapter that
/// can list its stores, finding one by address *is* reading the list, so the matching rule
/// ([`SharedMailbox::answers_to`]) and the failure it produces live here once instead of in
/// each such adapter.
///
/// `?Sized`, so it also serves a `Box<dyn Provider>`.
///
/// # Errors
///
/// Rejects as unsupported unless the adapter advertises
/// [`SharedMailboxes::Enumerable`]; otherwise propagates the listing's own failure, and
/// answers [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) when no
/// listed store answers to `address` — the server lists exactly what the credential was
/// granted, so an address missing from it is not one this credential can open, and no
/// retry will change that.
pub(crate) async fn resolve_by_listing<P: Provider + ?Sized>(
    provider: &P,
    address: &str,
) -> ProviderResult<SharedMailbox> {
    if provider.connection_info().capabilities.shared_mailboxes() != SharedMailboxes::Enumerable {
        return Err(unsupported("resolving a shared mailbox"));
    }
    provider
        .list_shared_mailboxes()
        .await?
        .into_iter()
        .find(|mailbox| mailbox.answers_to(address))
        .ok_or_else(|| {
            ProviderError::permanent(format!(
                "no mailbox shared with this credential answers to {address:?}"
            ))
        })
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
